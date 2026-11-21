// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/solidity.py
//! Port of graphify/extractors/solidity.py (Graphify 0.9.69, #3716):
//! contracts, interfaces and libraries with their state, events, errors,
//! structs, enums, functions, modifiers and imports. Inheritance and `using`
//! references bind across imported files through
//! [`resolve_solidity_type_references`]; calls, `super` calls and modifiers a
//! contract inherits bind through [`resolve_solidity_inherited_calls`]
//! (Graphify 0.9.92, #4379). A call on a typed receiver binds through the
//! receiver's contract or interface, and one a `using A for T` directive
//! attaches binds to the library or free function (Graphify 0.9.104, #5060,
//! #5062).
use super::base::{file_stem, make_id, read_text};
use super::engine_types::python_underscore_salted_nid;
use crate::{
    extract::per_file::PerFile,
    fsutil, io_compat, path_syntax, python_compat,
    resolver_registry::{Rec, append_edges, at, edge_record},
};
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;

const TYPE_KINDS: [&str; 3] = ["contract", "interface", "library"];
fn type_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "contract_declaration" => "contract",
        "interface_declaration" => "interface",
        "library_declaration" => "library",
        _ => return None,
    })
}
fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
/// `_unquote`: strip whitespace, then one pair of matching quotes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && matches!(chars[0], '\'' | '"') {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        value.into()
    }
}
/// `TypeContext`: a type's written text is read in the contract it was
/// written in, its name and the structs, enums and value types it declares
/// (Graphify 0.9.104).
type TypeContext = (Option<String>, BTreeSet<String>);
/// `VariableTypes`: variable name to (normalized type text, context); `None`
/// when the name is declared twice with different types.
type VariableTypes = HashMap<String, Option<(String, TypeContext)>>;
/// Collision groups for [`python_underscore_salted_nid`].
type Groups = BTreeMap<String, BTreeSet<String>>;

struct Extractor<'s> {
    source: &'s [u8],
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    seen: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
    /// `import {A as B} from "./A.sol";`: `B` -> `A`, so `B.f()` calls `A.f`
    /// (Graphify 0.9.103).
    import_aliases: HashMap<String, String>,
    /// The type context of each contract, interface and library, by node id.
    contract_contexts: HashMap<usize, TypeContext>,
    /// Struct fields by qualified struct name, for `s.field.f()`.
    struct_fields: HashMap<String, (TypeContext, HashMap<String, String>)>,
    /// The current type's member collision groups (`_grantRole` and
    /// `grantRole` share a plain id; Graphify 0.9.104).
    member_groups: Groups,
}

/// `_ELEMENTARY_ALIASES` and `_normalize_type`: one spelling per type,
/// whitespace collapsed (`mapping(a => b)` is `mapping(a=>b)`) and the
/// elementary aliases expanded (`uint` is `uint256`).
fn normalize_type(text: &str) -> String {
    static SPACE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());
    static PUNCT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r" ?([\[\]().,]|=>) ?").unwrap());
    static ALIAS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\b(uint|int|byte)\b").unwrap());
    let text = SPACE.replace_all(text, " ");
    let text = PUNCT.replace_all(text.trim(), "$1");
    ALIAS
        .replace_all(&text, |c: &regex::Captures<'_>| match &c[1] {
            "uint" => "uint256",
            "int" => "int256",
            _ => "bytes1",
        })
        .into_owned()
}

/// `_element_type`: the value type of `mapping(K=>V)` or the element type of
/// `T[]` / `T[n]` (normalized text).
fn element_type(text: &str) -> Option<String> {
    static VALUE_NAME: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^(.*\S) ([A-Za-z_$][A-Za-z0-9_$]*)$").unwrap());
    if let Some(inner) = text
        .strip_prefix("mapping(")
        .and_then(|t| t.strip_suffix(')'))
    {
        let mut depth = 0i32;
        for (index, ch) in inner.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ if depth == 0 && inner[index..].starts_with("=>") => {
                    let value = &inner[index + 2..];
                    // A named mapping value (`mapping(address a => uint256 b)`).
                    if let Some(c) = VALUE_NAME.captures(value)
                        && &c[2] != "payable"
                    {
                        return Some(c[1].to_string());
                    }
                    return Some(value.to_string());
                }
                _ => {}
            }
        }
        return None;
    }
    if text.ends_with(']') {
        let mut depth = 0i32;
        for (index, ch) in text.char_indices().rev() {
            if ch == ']' {
                depth += 1;
            } else if ch == '[' {
                depth -= 1;
                if depth == 0 {
                    return (index > 0).then(|| text[..index].to_string());
                }
            }
        }
    }
    None
}

/// `_BUILTIN_MEMBER_TYPES`: globals the language types itself.
fn builtin_member_type(written: &str) -> Option<&'static str> {
    Some(match written {
        "msg.sender" | "tx.origin" => "address",
        "msg.value" | "block.timestamp" | "block.number" | "block.chainid" => "uint256",
        _ => return None,
    })
}
/// `_BUILTIN_CALL_TYPES`: builtin calls whose result type is fixed.
fn builtin_call_type(written: &str) -> Option<&'static str> {
    Some(match written {
        "keccak256" | "sha256" | "blockhash" => "bytes32",
        "abi.encode"
        | "abi.encodePacked"
        | "abi.encodeWithSelector"
        | "abi.encodeWithSignature"
        | "abi.encodeCall" => "bytes",
        _ => return None,
    })
}

/// `_IDENTIFIER.match`: the identifier at the start of `text`, if any.
fn leading_identifier(text: &str) -> Option<&str> {
    let mut end = 0;
    for (i, c) in text.char_indices() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_' || c == '$'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        };
        if !ok {
            break;
        }
        end = i + c.len_utf8();
    }
    (end > 0).then(|| &text[..end])
}

/// `_rightmost_operand`: the receiver of a member call whose object
/// tree-sitter-solidity merged with a preceding operator (see
/// [`callee_text`]).
fn rightmost_operand(object: Node<'_>) -> Node<'_> {
    let mut base = object;
    loop {
        let unwrapped = if base.kind() == "expression" {
            named(base).into_iter().next().unwrap_or(base)
        } else {
            base
        };
        let right = match unwrapped.kind() {
            "binary_expression" => unwrapped.child_by_field_name("right"),
            "unary_expression" => unwrapped.child_by_field_name("argument"),
            _ => None,
        };
        let Some(right) = right else { return base };
        base = right;
    }
}

/// `_member_callee`: the `member_expression` of a member call `recv.f(..)`.
fn member_callee(callee: Node<'_>) -> Option<Node<'_>> {
    let inner = if callee.kind() == "expression" {
        named(callee).into_iter().next().unwrap_or(callee)
    } else {
        callee
    };
    (inner.kind() == "member_expression").then_some(inner)
}

/// `unwrap` in extract_solidity: through single-child expression and
/// parenthesized-expression wrappers.
fn unwrap_expression(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "expression" | "parenthesized_expression") {
        let kids = named(node);
        if kids.len() != 1 {
            break;
        }
        node = kids[0];
    }
    node
}

/// `_callable_signature`: `arity:types` of a function, modifier or
/// constructor, the part of its id that keeps overloads apart.
fn callable_signature(declaration: Node<'_>, source: &[u8]) -> String {
    let parameters = named(declaration)
        .into_iter()
        .filter(|c| matches!(c.kind(), "parameter" | "fallback_receive_parameter"))
        .collect::<Vec<_>>();
    let types = parameters
        .iter()
        .filter_map(|p| p.child_by_field_name("type"))
        .map(|t| read_text(t, source))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}:{types}", parameters.len())
}

/// `_member_identity`: `(kind, name, signature)` a contract member's node id
/// is made from, as `add_member` mints it; `None` for a member that gets no
/// id there.
fn member_identity(member: Node<'_>, source: &[u8]) -> Option<(&'static str, String, String)> {
    let simple = match member.kind() {
        "struct_declaration" => Some("struct"),
        "enum_declaration" => Some("enum"),
        "event_definition" => Some("event"),
        "error_declaration" => Some("error"),
        "state_variable_declaration" => Some("state_variable"),
        _ => None,
    };
    if let Some(kind) = simple {
        let name = read_text(member.child_by_field_name("name")?, source);
        return Some((kind, name.clone(), name));
    }
    match member.kind() {
        "constructor_definition" => Some((
            "constructor",
            "constructor".into(),
            callable_signature(member, source),
        )),
        "fallback_receive_definition" => {
            let name = if read_text(member, source)
                .trim_start()
                .starts_with("receive")
            {
                "receive"
            } else {
                "fallback"
            };
            Some((name, name.into(), callable_signature(member, source)))
        }
        "function_definition" | "modifier_definition" => {
            let name = read_text(member.child_by_field_name("name")?, source);
            let kind = if member.kind() == "modifier_definition" {
                "modifier"
            } else {
                "function"
            };
            Some((kind, name, callable_signature(member, source)))
        }
        _ => None,
    }
}

/// `_collision_groups`: `{plain id: {names}}` for the plain ids two or more
/// distinct names would share. `make_id` drops a leading `_` and folds case,
/// so `_grantRole(bytes32,address)` and `grantRole(bytes32,address)` in one
/// contract mint one id; an overload is one name, never a collision.
fn collision_groups(declared: impl IntoIterator<Item = (String, String)>) -> Groups {
    let mut groups = Groups::new();
    for (plain, name) in declared {
        groups.entry(plain).or_default().insert(name);
    }
    groups.retain(|_, names| names.len() >= 2);
    groups
}
/// `_IDENTIFIER`: a Solidity identifier, whole.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
/// `split_qualified` (Graphify 0.9.103): `("Lib", "f")` for `Lib.f`; `None`
/// for `a.b.f`, `x[i].f`, `f().g`, `super.f` or `this.f`.
fn split_qualified(written: &str) -> Option<(&str, &str)> {
    let (qualifier, member) = written.split_once('.')?;
    (!matches!(qualifier, "super" | "this") && is_identifier(qualifier) && is_identifier(member))
        .then_some((qualifier, member))
}
impl Extractor<'_> {
    fn text(&self, node: Node<'_>) -> String {
        read_text(node, self.source)
    }
    #[allow(clippy::too_many_arguments)]
    fn add_node(
        &mut self,
        id: &str,
        label: &str,
        node: Node<'_>,
        kind: &str,
        source_backed: bool,
        callable: bool,
        arity: Option<usize>,
        extra: Option<Map<String, Value>>,
    ) -> String {
        if self.seen.insert(id.into()) {
            let mut item = Map::new();
            item.insert("id".into(), json!(id));
            item.insert("label".into(), json!(label));
            item.insert("file_type".into(), json!("code"));
            item.insert(
                "source_location".into(),
                json!(format!("L{}", node.start_position().row + 1)),
            );
            let mut metadata = json!({"language":"solidity","kind":kind});
            if let Some(arity) = arity {
                // The parameter count, so the cross-contract pass picks the
                // overload the in-contract lookup would (name, arity).
                metadata["arity"] = json!(arity);
            }
            for (k, v) in extra.into_iter().flatten() {
                metadata[k.as_str()] = v;
            }
            item.insert("metadata".into(), metadata);
            if source_backed {
                item.insert("source_file".into(), json!(self.path));
            }
            if callable {
                item.insert("_callable".into(), json!(true));
            }
            self.nodes.push(Value::Object(item));
        }
        id.into()
    }
    fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        node: Node<'_>,
        context: Option<&str>,
        target_file: Option<&str>,
    ) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self
                .seen_edges
                .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        let mut item = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1),"weight":1.0});
        if let Some(context) = context.filter(|c| !c.is_empty()) {
            item["context"] = json!(context);
        }
        if let Some(target_file) = target_file.filter(|t| !t.is_empty()) {
            item["target_file"] = json!(target_file);
        }
        self.edges.push(item);
    }
    /// `_inherited_call`: a call or modifier the contract itself does not
    /// declare, for [`resolve_solidity_inherited_calls`]. `kind` is `inherited`
    /// (bare `f()` / `this.f()`, `emit E()`, `revert Err()`), `super`
    /// (`super.f()`), `modifier` or `qualified` (`Lib.f()`, `revert
    /// Errors.Err()`, with a `qualifier`; Graphify 0.9.103).
    fn inherited_call(
        &mut self,
        caller: &str,
        callee: &str,
        arity: Option<usize>,
        kind: &str,
        node: Node<'_>,
    ) {
        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"arity":arity,"solidity_call":kind,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
    }
    /// Parks `Qualifier.member(..)` for [`resolve_solidity_inherited_calls`],
    /// which binds it through a type the file declares or imports (Graphify
    /// 0.9.103).
    fn qualified_call(
        &mut self,
        caller: &str,
        qualifier: &str,
        member: &str,
        arity: usize,
        node: Node<'_>,
    ) {
        self.inherited_call(caller, member, Some(arity), "qualified", node);
        let qualifier = self
            .import_aliases
            .get(qualifier)
            .map_or(qualifier, String::as_str)
            .to_owned();
        if let Some(call) = self.raw_calls.last_mut() {
            call["qualifier"] = json!(qualifier);
        }
    }
    /// `emit E(..)` / `revert Err(..)` (Graphify 0.9.103): statements, not
    /// call expressions, that call the event or error they name; `revert()` and
    /// `revert("msg")` name none. Bound by (name, arity) in the contract,
    /// through its bases, or as `Lib.Err` through a declared or imported type.
    fn emitted_call(
        &mut self,
        node: Node<'_>,
        caller: &str,
        functions: &HashMap<(String, usize), Vec<String>>,
    ) {
        let emit = node.kind() == "emit_statement";
        let mut emitted = node.child_by_field_name(if emit { "name" } else { "error" });
        if let Some(e) = emitted
            && e.kind() == "expression"
            && let Some(first) = named(e).into_iter().next()
        {
            emitted = Some(first);
        }
        let args = if emit {
            Some(node)
        } else {
            named(node)
                .into_iter()
                .find(|c| c.kind() == "revert_arguments")
        };
        let arity = args.map_or(0, |a| {
            named(a)
                .into_iter()
                .filter(|c| c.kind() == "call_argument")
                .count()
        });
        let Some(emitted) = emitted else {
            return;
        };
        if emitted.kind() == "identifier" {
            let callee = self.text(emitted);
            match functions.get(&(callee.clone(), arity)).map(Vec::as_slice) {
                Some([target]) => {
                    let target = target.clone();
                    self.add_edge(caller, &target, "calls", node, None, None);
                }
                None | Some([]) => {
                    // Declared in a base (an interface's event, a base's
                    // error): bound through the linearization.
                    self.inherited_call(caller, &callee, Some(arity), "inherited", node);
                }
                _ => {}
            }
        } else if emitted.kind() == "member_expression"
            && let (Some(qualifier), Some(property)) = (
                emitted.child_by_field_name("object"),
                emitted.child_by_field_name("property"),
            )
            && qualifier.kind() == "identifier"
        {
            let (qualifier, property) = (self.text(qualifier), self.text(property));
            self.qualified_call(caller, &qualifier, &property, arity, node);
        }
    }
    /// `member_call`: park `recv.member(..)` for
    /// [`resolve_solidity_inherited_calls`], with the receiver's declared
    /// type and/or, for `g(..).member()`, the call whose result it is (typed
    /// there from `g`'s return type; Graphify 0.9.104, #5060).
    fn member_call(
        &mut self,
        caller: &str,
        member: &str,
        arity: usize,
        node: Node<'_>,
        receiver: (Option<String>, Option<(String, usize)>),
    ) {
        self.inherited_call(caller, member, Some(arity), "member", node);
        if let Some(call) = self.raw_calls.last_mut() {
            if let Some(typed) = receiver.0 {
                call["receiver_type"] = json!(typed);
            }
            if let Some((function, count)) = receiver.1 {
                call["receiver_call"] = json!([function, count]);
            }
        }
    }
    /// `qualify`: `text` as written in `context`, its leading type name
    /// qualified (`Trace208` in library Checkpoints is
    /// `Checkpoints.Trace208`) and an import alias replaced by the name it
    /// imports.
    fn qualify(&self, text: &str, context: &TypeContext) -> String {
        let Some(head) = leading_identifier(text) else {
            return text.into();
        };
        let rest = &text[head.len()..];
        if !rest.starts_with('.')
            && let Some(owner) = &context.0
            && context.1.contains(head)
        {
            return format!("{owner}.{head}{rest}");
        }
        format!(
            "{}{rest}",
            self.import_aliases.get(head).map_or(head, String::as_str)
        )
    }
    /// `using_directives`: the `using A for T;` / `using A for *;` /
    /// `using {f, L.g} for T [global];` directives directly in `scope`, as
    /// `{"for": T or "*", "library": A}` or `{"for": T, "functions": [..]}`,
    /// plus `"global": true`. An operator binding (`using {eq as ==} for T`)
    /// attaches no member and is skipped.
    fn using_directives(&self, scope: Node<'_>, context: &TypeContext) -> Vec<Value> {
        let file_context: TypeContext = (None, BTreeSet::new());
        let mut directives = vec![];
        for directive in named(scope) {
            if directive.kind() != "using_directive" {
                continue;
            }
            let Some(target) = directive.child_by_field_name("source") else {
                continue;
            };
            let mut item = Map::new();
            item.insert(
                "for".into(),
                json!(if target.kind() == "any_source_type" {
                    "*".to_string()
                } else {
                    self.qualify(&normalize_type(&self.text(target)), context)
                }),
            );
            if let Some(library) = named(directive)
                .into_iter()
                .find(|c| c.kind() == "type_alias")
            {
                item.insert(
                    "library".into(),
                    json!(self.qualify(&normalize_type(&self.text(library)), &file_context)),
                );
            } else {
                let mut functions = vec![];
                for alias in named(directive) {
                    if alias.kind() != "using_alias" {
                        continue;
                    }
                    let mut cursor = alias.walk();
                    if alias.children(&mut cursor).any(|c| c.kind() == "as") {
                        continue;
                    }
                    for bound in named(alias) {
                        if bound.kind() == "user_defined_type" {
                            functions.push(normalize_type(&self.text(bound)));
                        }
                    }
                }
                if functions.is_empty() {
                    continue;
                }
                item.insert(
                    "functions".into(),
                    json!(
                        functions
                            .iter()
                            .map(|f| self.qualify(f, &file_context))
                            .collect::<Vec<_>>()
                    ),
                );
            }
            let mut cursor = directive.walk();
            if directive
                .children(&mut cursor)
                .any(|c| c.kind() == "global")
            {
                item.insert("global".into(), json!(true));
            }
            directives.push(Value::Object(item));
        }
        directives
    }
    /// `first_parameter_type`: `{"first_param_type": T}` for a library or
    /// free function, the type a `using` directive attaches it to.
    fn first_parameter_type(
        &self,
        callable: Node<'_>,
        context: &TypeContext,
    ) -> Map<String, Value> {
        let mut out = Map::new();
        if let Some(type_node) = named(callable)
            .into_iter()
            .find(|c| c.kind() == "parameter")
            .and_then(|p| p.child_by_field_name("type"))
        {
            out.insert(
                "first_param_type".into(),
                json!(self.qualify(&normalize_type(&self.text(type_node)), context)),
            );
        }
        out
    }
    /// `declared_types`: parameter, return and local variable types visible
    /// in a function or modifier, over `inherited` (the contract's state
    /// variables). A name declared twice with different types is ambiguous.
    fn declared_types(
        &self,
        callable: Node<'_>,
        context: &TypeContext,
        inherited: &VariableTypes,
    ) -> VariableTypes {
        let mut found: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut stack = vec![callable];
        while let Some(current) = stack.pop() {
            if matches!(current.kind(), "parameter" | "variable_declaration")
                && let (Some(type_node), Some(name_node)) = (
                    current.child_by_field_name("type"),
                    current.child_by_field_name("name"),
                )
            {
                found
                    .entry(self.text(name_node))
                    .or_default()
                    .insert(normalize_type(&self.text(type_node)));
            }
            stack.extend(named(current));
        }
        let mut scope = inherited.clone();
        for (name, types) in found {
            let one =
                (types.len() == 1).then(|| (types.into_iter().next().unwrap(), context.clone()));
            scope.insert(name, one);
        }
        scope
    }
    /// `typed_expression`: the declared type of an expression and the context
    /// it reads in.
    fn typed_expression(
        &self,
        node: Node<'_>,
        scope: &VariableTypes,
        context: &TypeContext,
        local_functions: &HashSet<String>,
    ) -> Option<(String, TypeContext)> {
        let file_context = || (None, BTreeSet::new());
        match node.kind() {
            "identifier" => scope.get(&self.text(node)).cloned().flatten(),
            "array_access" => {
                let base = node.child_by_field_name("base")?;
                let typed = self.typed_expression(
                    unwrap_expression(base),
                    scope,
                    context,
                    local_functions,
                )?;
                let element = element_type(&typed.0)?;
                (!element.is_empty()).then_some((element, typed.1))
            }
            "member_expression" => {
                let written = self.text(node);
                if let Some(t) = builtin_member_type(&written) {
                    return Some((t.into(), file_context()));
                }
                let object = node.child_by_field_name("object")?;
                let typed = self.typed_expression(
                    unwrap_expression(object),
                    scope,
                    context,
                    local_functions,
                )?;
                let (struct_context, fields) =
                    self.struct_fields.get(&self.qualify(&typed.0, &typed.1))?;
                let field = fields.get(&self.text(node.child_by_field_name("property")?))?;
                (!field.is_empty()).then(|| (field.clone(), struct_context.clone()))
            }
            "type_cast_expression" => {
                let type_node = named(node)
                    .into_iter()
                    .find(|c| c.kind() == "primitive_type")?;
                Some((normalize_type(&self.text(type_node)), file_context()))
            }
            "payable_conversion_expression" => Some(("address payable".into(), file_context())),
            "call_expression" => {
                let function = node.child_by_field_name("function")?;
                let written = normalize_type(&self.text(unwrap_expression(function)));
                if let Some(t) = builtin_call_type(&written) {
                    return Some((t.into(), file_context()));
                }
                let arguments = named(node)
                    .into_iter()
                    .filter(|c| c.kind() == "call_argument")
                    .count();
                // `T(x)`: a conversion to the type `T`. A function of the
                // caller's own contract or a variable is not a type; any other
                // name is checked against the declared types by the resolver.
                (arguments == 1
                    && is_identifier(&written)
                    && !scope.contains_key(&written)
                    && !local_functions.contains(&written))
                .then(|| (written, context.clone()))
            }
            _ => None,
        }
    }
    /// `member_receiver`: `(declared type, (g, arity))` of a member call's
    /// receiver, either part `None` when unknown; `None` when both are. The
    /// declared type is a typed variable, an element of a typed mapping or
    /// array, a field of a struct declared in this file, a conversion
    /// (`IERC20(a)`, `uint256(x)`) or a builtin; `(g, arity)` when the
    /// receiver is the result of a bare call `g(..)`.
    #[allow(clippy::type_complexity)]
    fn member_receiver(
        &self,
        callee: Node<'_>,
        scope: &VariableTypes,
        context: &TypeContext,
        local_functions: &HashSet<String>,
    ) -> Option<(Option<String>, Option<(String, usize)>)> {
        let object = member_callee(callee)?.child_by_field_name("object")?;
        let receiver = unwrap_expression(rightmost_operand(object));
        let typed = self
            .typed_expression(receiver, scope, context, local_functions)
            .map(|(t, c)| self.qualify(&t, &c));
        let result_of = (receiver.kind() == "call_expression")
            .then(|| receiver.child_by_field_name("function"))
            .flatten()
            .map(|f| self.text(unwrap_expression(f)))
            .filter(|name| {
                is_identifier(name)
                    && !scope.contains_key(name)
                    && builtin_call_type(name).is_none()
            })
            .map(|name| {
                let count = named(receiver)
                    .into_iter()
                    .filter(|c| c.kind() == "call_argument")
                    .count();
                (name, count)
            });
        (typed.is_some() || result_of.is_some()).then_some((typed, result_of))
    }
    fn type_stub(&mut self, name: &str, node: Node<'_>) -> String {
        let name = name.rsplit('.').next().unwrap_or("").to_string();
        let id = make_id(["solidity", "type", name.as_str()]);
        self.add_node(&id, &name, node, "external_type", false, false, None, None)
    }
    /// Callables carry an `arity:types` signature to tell overloads apart;
    /// other members are unique per kind by name within a contract.
    #[allow(clippy::too_many_arguments)]
    fn add_member(
        &mut self,
        type_id: &str,
        member: Node<'_>,
        name: &str,
        label: &str,
        kind: &str,
        relation: &str,
        signature: &str,
        callable: bool,
        arity: Option<usize>,
        extra: Option<Map<String, Value>>,
    ) -> String {
        let discriminator = if signature.is_empty() {
            name
        } else {
            signature
        };
        // Members whose names differ only by a leading `_` (or by case) share
        // a plain id; the one public name keeps it and the others are salted,
        // as Python and C# do (Graphify 0.9.104; #3302, #4415).
        let id = python_underscore_salted_nid(
            &make_id([type_id, kind, name, discriminator]),
            name,
            &self.member_groups,
        );
        self.add_node(&id, label, member, kind, true, callable, arity, extra);
        self.add_edge(type_id, &id, relation, member, None, None);
        id
    }
    fn declaration(
        &mut self,
        declaration: Node<'_>,
        kind: &str,
        stem: &str,
        file_id: &str,
        type_groups: &Groups,
    ) {
        let (Some(name_node), Some(body)) = (
            declaration.child_by_field_name("name"),
            declaration.child_by_field_name("body"),
        ) else {
            return;
        };
        let type_name = self.text(name_node);
        let is_library = kind == "library";
        let type_context = self
            .contract_contexts
            .get(&declaration.id())
            .cloned()
            .unwrap_or_default();
        let contract_using = self.using_directives(body, &type_context);
        // Read by the resolver to bind `x.f()` through `using A for T`.
        let extra = (!contract_using.is_empty()).then(|| {
            let mut m = Map::new();
            m.insert("using".into(), json!(contract_using));
            m
        });
        let type_id = self.add_node(
            &python_underscore_salted_nid(
                &make_id([stem, kind, type_name.as_str()]),
                &type_name,
                type_groups,
            ),
            &type_name,
            declaration,
            kind,
            true,
            kind == "contract",
            None,
            extra,
        );
        self.add_edge(file_id, &type_id, "contains", declaration, None, None);
        for child in named(declaration) {
            if child.kind() != "inheritance_specifier" {
                continue;
            }
            let Some(ancestor) = child.child_by_field_name("ancestor") else {
                continue;
            };
            let base = self
                .text(ancestor)
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string();
            let stub = self.type_stub(&base, child);
            let context = format!("solidity_type:{base}");
            self.add_edge(&type_id, &stub, "inherits", child, Some(&context), None);
        }
        let mut functions: HashMap<(String, usize), Vec<String>> = HashMap::new();
        let mut modifiers: HashMap<String, String> = HashMap::new();
        let mut references: HashMap<(String, usize), String> = HashMap::new();
        let mut bodies = vec![];
        let mut state_types: VariableTypes = HashMap::new();
        let mut pending_modifiers = vec![];
        self.member_groups = collision_groups(named(body).into_iter().filter_map(|member| {
            let (kind, name, signature) = member_identity(member, self.source)?;
            Some((make_id([type_id.as_str(), kind, &name, &signature]), name))
        }));
        for member in named(body) {
            let kind = member.kind();
            if kind == "using_directive" {
                // `using Lib for T;` names the library as a child; `using {f1,
                // f2} for T;` wraps each function in a `using_alias` (Graphify
                // 0.9.98): every one is used.
                let is_alias = |c: &Node<'_>| {
                    matches!(c.kind(), "type_alias" | "identifier" | "user_defined_type")
                };
                let mut aliases = named(member)
                    .into_iter()
                    .filter(is_alias)
                    .collect::<Vec<_>>();
                for child in named(member) {
                    if child.kind() == "using_alias" {
                        aliases.extend(named(child).into_iter().filter(is_alias));
                    }
                }
                for alias in aliases {
                    let alias = self.text(alias);
                    if alias.is_empty() {
                        continue;
                    }
                    let name = alias.rsplit('.').next().unwrap_or("").to_string();
                    let stub = self.type_stub(&name, member);
                    let context = format!("solidity_type:{name}");
                    self.add_edge(&type_id, &stub, "uses", member, Some(&context), None);
                }
                continue;
            }
            if kind == "struct_declaration" || kind == "enum_declaration" {
                let Some(name_node) = member.child_by_field_name("name") else {
                    continue;
                };
                let name = self.text(name_node);
                let member_body = member.child_by_field_name("body");
                let is_struct = kind == "struct_declaration";
                let owner = self.add_member(
                    &type_id,
                    member,
                    &name,
                    &name,
                    if is_struct { "struct" } else { "enum" },
                    "contains",
                    "",
                    false,
                    None,
                    None,
                );
                for item in member_body.map(named).unwrap_or_default() {
                    if is_struct {
                        if item.kind() != "struct_member" {
                            continue;
                        }
                        let Some(field) = item.child_by_field_name("name") else {
                            continue;
                        };
                        let field = self.text(field);
                        let id = make_id([owner.as_str(), "field", field.as_str()]);
                        self.add_node(&id, &field, item, "field", true, false, None, None);
                        self.add_edge(&owner, &id, "contains", item, None, None);
                    } else {
                        if item.kind() != "enum_value" {
                            continue;
                        }
                        let value = self.text(item);
                        let id = make_id([owner.as_str(), value.as_str()]);
                        self.add_node(&id, &value, item, "enum_value", true, false, None, None);
                        // A value is a discriminant case, not a declared member;
                        // constructor binding skips case_of targets (0.9.77).
                        self.add_edge(&owner, &id, "case_of", item, None, None);
                    }
                }
                continue;
            }
            let simple = match kind {
                "event_definition" => Some("event"),
                "error_declaration" => Some("error"),
                "state_variable_declaration" => Some("state_variable"),
                _ => None,
            };
            if let Some(simple) = simple {
                let Some(name_node) = member.child_by_field_name("name") else {
                    continue;
                };
                let name = self.text(name_node);
                let arity = named(member)
                    .into_iter()
                    .filter(|c| {
                        matches!(
                            c.kind(),
                            "event_parameter" | "error_parameter" | "parameter"
                        )
                    })
                    .count();
                if simple == "state_variable" {
                    let mut declared = Map::new();
                    if let Some(type_node) = member.child_by_field_name("type") {
                        let state_type = normalize_type(&self.text(type_node));
                        // Read by the resolver for a receiver that is a base
                        // contract's state variable (`hevm.prank()`).
                        declared.insert(
                            "declared_type".into(),
                            json!(self.qualify(&state_type, &type_context)),
                        );
                        state_types.insert(name.clone(), Some((state_type, type_context.clone())));
                    }
                    let id = self.add_member(
                        &type_id,
                        member,
                        &name,
                        &name,
                        simple,
                        "contains",
                        "",
                        false,
                        None,
                        (!declared.is_empty()).then_some(declared),
                    );
                    references.insert((name, arity), id);
                } else {
                    // `emit E(..)`, `revert Err(..)` and `require(c, Err(..))`
                    // call the event or error, like Python's `raise Err()`: a
                    // call target by (name, arity) (Graphify 0.9.103).
                    let id = self.add_member(
                        &type_id,
                        member,
                        &name,
                        &name,
                        simple,
                        "contains",
                        "",
                        false,
                        Some(arity),
                        None,
                    );
                    functions.entry((name, arity)).or_default().push(id);
                }
                continue;
            }
            if !matches!(
                kind,
                "function_definition"
                    | "constructor_definition"
                    | "modifier_definition"
                    | "fallback_receive_definition"
            ) {
                continue;
            }
            let (name, callable_kind) = match kind {
                "constructor_definition" => ("constructor".to_string(), "constructor"),
                "fallback_receive_definition" => {
                    if self.text(member).trim_start().starts_with("receive") {
                        ("receive".to_string(), "receive")
                    } else {
                        ("fallback".to_string(), "fallback")
                    }
                }
                _ => {
                    let Some(name_node) = member.child_by_field_name("name") else {
                        continue;
                    };
                    let callable_kind = if kind == "modifier_definition" {
                        "modifier"
                    } else {
                        "function"
                    };
                    (self.text(name_node), callable_kind)
                }
            };
            let parameters = named(member)
                .into_iter()
                .filter(|c| matches!(c.kind(), "parameter" | "fallback_receive_parameter"))
                .collect::<Vec<_>>();
            let signature = parameters
                .iter()
                .filter_map(|p| p.child_by_field_name("type"))
                .map(|t| self.text(t))
                .collect::<Vec<_>>()
                .join(",");
            let arity = parameters.len();
            let mut returned = Map::new();
            let return_types = named(member)
                .into_iter()
                .filter(|c| c.kind() == "return_type_definition")
                .flat_map(named)
                .filter(|p| p.kind() == "parameter")
                .map(|p| p.child_by_field_name("type"))
                .collect::<Vec<_>>();
            if is_library && callable_kind == "function" {
                returned.extend(self.first_parameter_type(member, &type_context));
            }
            if let [Some(returns)] = return_types.as_slice() {
                // Read by the resolver for a receiver that is this function's
                // result (`token().clock()`).
                returned.insert(
                    "returns".into(),
                    json!(self.qualify(&normalize_type(&self.text(*returns)), &type_context)),
                );
            }
            let id = self.add_member(
                &type_id,
                member,
                &name,
                &format!("{name}()"),
                callable_kind,
                "method",
                &format!("{arity}:{signature}"),
                true,
                Some(arity),
                (!returned.is_empty()).then_some(returned),
            );
            if callable_kind == "modifier" {
                modifiers.insert(name.clone(), id.clone());
            } else {
                functions
                    .entry((name.clone(), arity))
                    .or_default()
                    .push(id.clone());
            }
            if let Some(member_body) = member.child_by_field_name("body") {
                bodies.push((member_body, id.clone(), member));
            }
            // A constructor's `ERC20("n", "s")` base call is a modifier
            // invocation too (0.9.92).
            if callable_kind != "modifier" {
                for child in named(member)
                    .into_iter()
                    .filter(|c| c.kind() == "modifier_invocation")
                {
                    let modifier = named(child)
                        .into_iter()
                        .find(|c| c.kind() == "identifier")
                        .map(|c| self.text(c))
                        .unwrap_or_default();
                    if !modifier.is_empty() {
                        pending_modifiers.push((id.clone(), modifier, child));
                    }
                }
            }
        }
        for (caller, modifier, node) in pending_modifiers {
            if let Some(target) = modifiers.get(&modifier).cloned() {
                self.add_edge(&caller, &target, "uses", node, None, None);
            } else {
                // Declared in a base contract (`onlyOwner` from Ownable), or a
                // base-constructor call the inheritance pass finds no modifier for.
                self.inherited_call(&caller, &modifier, None, "modifier", node);
            }
        }
        let own_functions = functions
            .keys()
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        for (body, caller, callable) in bodies {
            let scope = self.declared_types(callable, &type_context, &state_types);
            let mut stack = vec![body];
            while let Some(node) = stack.pop() {
                if node.kind() == "call_expression"
                    && let Some(callee_node) = node.child_by_field_name("function")
                {
                    let written = callee_text(callee_node, self.source);
                    let callee = written.rsplit('.').next().unwrap_or("").to_string();
                    let arity = named(node)
                        .into_iter()
                        .filter(|c| c.kind() == "call_argument")
                        .count();
                    let dotted = written.contains('.');
                    let candidates = if !dotted || written.starts_with("this.") {
                        functions
                            .get(&(callee.clone(), arity))
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        vec![]
                    };
                    if let [target] = candidates.as_slice() {
                        self.add_edge(&caller, target, "calls", node, None, None);
                    } else if let Some(referenced) = references
                        .get(&(callee.clone(), arity))
                        .filter(|_| !dotted)
                        .cloned()
                    {
                        self.add_edge(&caller, &referenced, "references", node, None, None);
                    } else if candidates.is_empty()
                        && !callee.is_empty()
                        && (!dotted || written.starts_with("this."))
                    {
                        // Not declared in this contract: an inherited function
                        // (`_msgSender()` from Context), bound through the
                        // contract's linearized bases.
                        self.inherited_call(&caller, &callee, Some(arity), "inherited", node);
                    } else if written.starts_with("super.") && written.matches('.').count() == 1 {
                        self.inherited_call(&caller, &callee, Some(arity), "super", node);
                    } else if let Some(receiver) = (!callee.is_empty())
                        .then(|| {
                            self.member_receiver(callee_node, &scope, &type_context, &own_functions)
                        })
                        .flatten()
                    {
                        // `token.transfer()` on a typed receiver (Graphify
                        // 0.9.104, #5060, #5062).
                        self.member_call(&caller, &callee, arity, node, receiver);
                    } else if let Some((qualifier, member)) =
                        split_qualified(&written).filter(|_| !callee.is_empty())
                    {
                        // `Lib.f(..)` / `Base.f(..)` / `Errors.Err(..)`; a value
                        // receiver (`token.transfer()`) names no type, so the
                        // resolver binds nothing for it (Graphify 0.9.103).
                        self.qualified_call(&caller, qualifier, member, arity, node);
                    } else if !callee.is_empty() && dotted {
                        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                    }
                } else if matches!(node.kind(), "emit_statement" | "revert_statement") {
                    self.emitted_call(node, &caller, &functions);
                }
                let mut kids = named(node);
                kids.reverse();
                stack.extend(kids);
            }
        }
    }
    /// File-level (free) functions, legal since Solidity 0.7, and the calls
    /// they make. Only an unambiguous same-file free function is linked
    /// directly; a qualified call (`Lib.f()`) is left for
    /// [`resolve_solidity_inherited_calls`] (Graphify 0.9.103).
    fn free_functions(&mut self, root: Node<'_>, stem: &str, file_id: &str) {
        let file_context: TypeContext = (None, BTreeSet::new());
        let mut functions: HashMap<(String, usize), Vec<String>> = HashMap::new();
        let mut bodies = vec![];
        let free_groups = collision_groups(named(root).into_iter().filter_map(|declaration| {
            if declaration.kind() != "function_definition" {
                return None;
            }
            let name = self.text(declaration.child_by_field_name("name")?);
            let signature = callable_signature(declaration, self.source);
            Some((make_id([stem, "function", &name, &signature]), name))
        }));
        for declaration in named(root) {
            if declaration.kind() != "function_definition" {
                continue;
            }
            let Some(name_node) = declaration.child_by_field_name("name") else {
                continue;
            };
            let name = self.text(name_node);
            let parameters = named(declaration)
                .into_iter()
                .filter(|c| c.kind() == "parameter")
                .collect::<Vec<_>>();
            let signature = parameters
                .iter()
                .filter_map(|p| p.child_by_field_name("type"))
                .map(|t| self.text(t))
                .collect::<Vec<_>>()
                .join(",");
            let arity = parameters.len();
            let extra = self.first_parameter_type(declaration, &file_context);
            let id = self.add_node(
                &python_underscore_salted_nid(
                    &make_id([
                        stem,
                        "function",
                        name.as_str(),
                        format!("{arity}:{signature}").as_str(),
                    ]),
                    &name,
                    &free_groups,
                ),
                &format!("{name}()"),
                declaration,
                "function",
                true,
                true,
                Some(arity),
                (!extra.is_empty()).then_some(extra),
            );
            self.add_edge(file_id, &id, "contains", declaration, None, None);
            functions.entry((name, arity)).or_default().push(id.clone());
            if let Some(body) = declaration.child_by_field_name("body") {
                bodies.push((body, id, declaration));
            }
        }
        let free_names = functions
            .keys()
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        for (body, caller, callable) in bodies {
            let scope = self.declared_types(callable, &file_context, &HashMap::new());
            let mut stack = vec![body];
            while let Some(node) = stack.pop() {
                if node.kind() == "call_expression"
                    && let Some(callee_node) = node.child_by_field_name("function")
                {
                    let written = callee_text(callee_node, self.source);
                    let callee = written.rsplit('.').next().unwrap_or("").to_string();
                    let arity = named(node)
                        .into_iter()
                        .filter(|c| c.kind() == "call_argument")
                        .count();
                    if !written.contains('.') {
                        if let Some([target]) = functions.get(&(callee, arity)).map(Vec::as_slice) {
                            let target = target.clone();
                            self.add_edge(&caller, &target, "calls", node, None, None);
                        }
                    } else if let Some(receiver) = (!callee.is_empty())
                        .then(|| {
                            self.member_receiver(callee_node, &scope, &file_context, &free_names)
                        })
                        .flatten()
                    {
                        self.member_call(&caller, &callee, arity, node, receiver);
                    } else if let Some((qualifier, member)) =
                        split_qualified(&written).filter(|_| !callee.is_empty())
                    {
                        self.qualified_call(&caller, qualifier, member, arity, node);
                    } else if !callee.is_empty() {
                        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                    }
                }
                let mut kids = named(node);
                kids.reverse();
                stack.extend(kids);
            }
        }
    }
}
/// `_callee_text`: the callee as written (`super.supportsInterface`,
/// `_check`). The grammar parses `a || super.f(x)` as a call of the member
/// `(a || super).f`; the right-most operand is read as the receiver instead.
/// A prefix operator is swallowed the same way: `!Lib.f(x)` parses as
/// `(!Lib).f` (Graphify 0.9.103).
fn callee_text(callee: Node<'_>, source: &[u8]) -> String {
    fn unwrap<'t>(node: Node<'t>) -> Node<'t> {
        if node.kind() == "expression" {
            named(node).into_iter().next().unwrap_or(node)
        } else {
            node
        }
    }
    let inner = unwrap(callee);
    if inner.kind() == "member_expression"
        && let (Some(object), Some(property)) = (
            inner.child_by_field_name("object"),
            inner.child_by_field_name("property"),
        )
    {
        let mut base = object;
        loop {
            let unwrapped = unwrap(base);
            let right = match unwrapped.kind() {
                "binary_expression" => unwrapped.child_by_field_name("right"),
                "unary_expression" => unwrapped.child_by_field_name("argument"),
                _ => None,
            };
            let Some(right) = right else { break };
            base = right;
        }
        if base != object {
            return format!(
                "{}.{}",
                read_text(base, source),
                read_text(property, source)
            );
        }
    }
    read_text(callee, source)
}
/// `_c3_linearization`: Solidity's C3 linearization of `type_id`, most derived
/// first. Each type's direct bases are in declaration order and the last is
/// the most derived, so they merge right to left. A cycle or an inconsistent
/// hierarchy falls back to a right-to-left depth-first order.
fn c3_linearization(type_id: &str, bases: &HashMap<String, Vec<String>>) -> Vec<String> {
    fn linearize(
        tid: &str,
        active: &mut Vec<String>,
        bases: &HashMap<String, Vec<String>>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Option<Vec<String>> {
        if let Some(done) = memo.get(tid) {
            return Some(done.clone());
        }
        if active.iter().any(|a| a == tid) {
            return None;
        }
        let parents = bases
            .get(tid)
            .map(|b| b.iter().rev().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut sequences: Vec<Vec<String>> = vec![];
        active.push(tid.to_owned());
        for parent in &parents {
            let lin = linearize(parent, active, bases, memo);
            let Some(lin) = lin else {
                active.pop();
                return None;
            };
            sequences.push(lin);
        }
        active.pop();
        sequences.push(parents);
        let mut result = vec![tid.to_owned()];
        while sequences.iter().any(|s| !s.is_empty()) {
            let head = sequences
                .iter()
                .filter_map(|seq| seq.first())
                .find(|candidate| {
                    !sequences
                        .iter()
                        .any(|other| other.iter().skip(1).any(|o| o == *candidate))
                })
                .cloned()?;
            for seq in &mut sequences {
                if seq.first() == Some(&head) {
                    seq.remove(0);
                }
            }
            result.push(head);
        }
        memo.insert(tid.to_owned(), result.clone());
        Some(result)
    }
    let mut memo = HashMap::new();
    if let Some(linear) = linearize(type_id, &mut vec![], bases, &mut memo) {
        return linear;
    }
    let mut order: Vec<String> = vec![];
    let mut stack = vec![type_id.to_owned()];
    while let Some(tid) = stack.pop() {
        if order.contains(&tid) {
            continue;
        }
        stack.extend(bases.get(&tid).into_iter().flatten().cloned());
        order.push(tid);
    }
    order
}
/// `resolve_solidity_inherited_calls` (Graphify 0.9.92, #4379): bind calls,
/// `super` calls and modifiers a contract inherits. Runs after the type pass
/// has bound each `inherits` edge to the base's real node, and walks the
/// caller contract's C3 linearization as the compiler does: the first contract
/// that declares a matching member wins (`super.f()` starts after the caller's
/// own contract, as the plain lookup does once the extractor found nothing
/// there); several same-arity overloads in that contract are ambiguous.
///
/// An event or error is a call target too (`emit E()`, `revert Err()`), and
/// a qualified call (`Lib.f()`, `Base.f()`, `revert Errors.Err()`) binds to
/// the member of the one library, contract or interface of that name the
/// caller's file declares or imports directly; no edge when the name or the
/// (name, arity) is ambiguous (Graphify 0.9.103).
pub fn resolve_solidity_inherited_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    resolve_solidity_inherited_calls_any(per, nodes, edges);
}
/// [`resolve_solidity_inherited_calls`] over the per-file results and the
/// edges in either form; new edges are appended after the loop.
pub fn resolve_solidity_inherited_calls_any<P: PerFile, E: Rec>(
    per: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) {
    let calls = per
        .iter()
        .flat_map(|r| r.raw_calls().into_iter().flatten())
        .filter(|rc| {
            rc.rec_get("solidity_call")
                .is_some_and(crate::build::truthy)
        })
        .collect::<Vec<_>>();
    if calls.is_empty() {
        return;
    }
    let mut node_by_id: HashMap<String, &Value> = HashMap::new();
    for node in nodes {
        node_by_id.insert(node["id"].to_string(), node);
    }
    let mut r = Resolver {
        node_by_id,
        empty: Value::Null,
        owner: HashMap::new(),
        functions: HashMap::new(),
        modifiers: HashMap::new(),
        bases: HashMap::new(),
        using: HashMap::new(),
        state_variables: HashMap::new(),
        types_by_file: HashMap::new(),
        imported: HashMap::new(),
        file_using: HashMap::new(),
        global_using: vec![],
        free_functions: HashMap::new(),
        reachable_cache: RefCell::new(HashMap::new()),
        linear_cache: RefCell::new(HashMap::new()),
    };
    let mut existing: HashSet<(String, String, String)> = HashSet::new();
    for edge in edges.iter() {
        let (source, target, relation) =
            (at(edge, "source"), at(edge, "target"), at(edge, "relation"));
        existing.insert((source.to_string(), target.to_string(), relation.to_string()));
        let (Some(source_id), Some(target_id)) = (source.as_str(), target.as_str()) else {
            continue;
        };
        let source_meta = r.meta(source);
        if source_meta["language"] != "solidity" || !is_type_meta(source_meta) {
            continue;
        }
        if let Some(using) = source_meta["using"].as_array().cloned() {
            r.using.insert(source_id.to_owned(), using);
        }
        if relation == "method" {
            let member = r.meta(target).clone();
            let label = r.label(target);
            let name = label.strip_suffix("()").unwrap_or(&label).to_owned();
            r.owner.insert(target_id.to_owned(), source_id.to_owned());
            if member["kind"] == "function" {
                r.functions
                    .entry(source_id.to_owned())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push((target_id.to_owned(), member["arity"].clone()));
            } else if member["kind"] == "modifier" {
                r.modifiers
                    .entry(source_id.to_owned())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push(target_id.to_owned());
            }
        } else if relation == "contains" && r.meta(target)["kind"] == "state_variable" {
            let label = r.label(target);
            r.state_variables
                .entry(source_id.to_owned())
                .or_default()
                .insert(label, target_id.to_owned());
        } else if relation == "contains"
            && matches!(r.meta(target)["kind"].as_str(), Some("event" | "error"))
        {
            let label = r.label(target);
            let arity = r.meta(target)["arity"].clone();
            r.functions
                .entry(source_id.to_owned())
                .or_default()
                .entry(label)
                .or_default()
                .push((target_id.to_owned(), arity));
        } else if relation == "inherits" && is_type_meta(r.meta(target)) {
            r.bases
                .entry(source_id.to_owned())
                .or_default()
                .push(target_id.to_owned());
        }
    }
    // `Errors.Err` names a type the caller's file declares or imports
    // directly, the scope the type pass binds base names in; `using`
    // directives at file level, the `global` ones by type, and free
    // functions by (file, name) for `using {f} for T` (Graphify 0.9.104).
    if calls.iter().any(|rc| {
        matches!(
            at(*rc, "solidity_call").as_str(),
            Some("qualified" | "member")
        )
    }) {
        let mut file_by_id: HashMap<String, String> = HashMap::new();
        for node in nodes {
            let Some(node_file) = node.get("source_file").filter(|v| crate::build::truthy(v))
            else {
                continue;
            };
            let node_file = python_compat::string(node_file);
            let label = node
                .get("label")
                .map(python_compat::string)
                .unwrap_or_default();
            let base_name = Path::new(&node_file)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if label == base_name {
                file_by_id.insert(node["id"].to_string(), node_file.clone());
                if let Some(directives) = r.meta(&node["id"])["using"].as_array().cloned() {
                    for d in &directives {
                        if crate::build::truthy(&d["global"]) {
                            r.global_using.push((d.clone(), node_file.clone()));
                        }
                    }
                    r.file_using.insert(node_file, directives);
                }
            } else if is_type_meta(r.meta(&node["id"])) {
                r.types_by_file
                    .entry((node_file, label))
                    .or_default()
                    .push(python_compat::string(&node["id"]));
            }
        }
        for edge in edges.iter() {
            if at(edge, "relation") == "imports_from" {
                if let (Some(importer), Some(imported_file)) = (
                    file_by_id.get(&at(edge, "source").to_string()),
                    file_by_id.get(&at(edge, "target").to_string()),
                ) {
                    r.imported
                        .entry(importer.clone())
                        .or_default()
                        .insert(imported_file.clone());
                }
            } else if at(edge, "relation") == "contains"
                && let Some(file) = file_by_id.get(&at(edge, "source").to_string())
                && r.meta(at(edge, "target"))["kind"] == "function"
            {
                let name = r.label(at(edge, "target"));
                let name = name.strip_suffix("()").unwrap_or(&name).to_owned();
                r.free_functions
                    .entry((file.clone(), name))
                    .or_default()
                    .push(python_compat::string(at(edge, "target")));
            }
        }
    }
    let mut added: Vec<E> = vec![];
    for rc in calls {
        let caller = at(rc, "caller_nid");
        let name = at(rc, "callee");
        if !crate::build::truthy(caller) || !crate::build::truthy(name) {
            continue;
        }
        let name = python_compat::string(name);
        let kind = python_compat::string(at(rc, "solidity_call"));
        let contract = caller.as_str().and_then(|c| r.owner.get(c)).cloned();
        let target = if kind == "member" {
            // A type's own member beats a `using`-attached function
            // (Solidity resolution precedence), so try the typed receiver
            // first (Graphify 0.9.104).
            r.typed_member(rc, &name)
                .or_else(|| r.using_member(rc, &name))
        } else if kind == "qualified" {
            // `Lib.f()` names its type, so it needs no owning contract (a
            // free function calls libraries too).
            let caller_file = r.caller_file(caller);
            let qualifier = python_compat::string(at(rc, "qualifier"));
            let scope = r.visible_types(&caller_file, &qualifier, false);
            // Fail closed when the name is not visible or is ambiguous. A
            // name that is no type may be a base contract's state variable
            // (`hevm.prank()`), or a value with a `using`-attached function
            // (`_checkpoints.push()`; Graphify 0.9.104).
            match scope.len() {
                1 => r.find(&scope, &name, at(rc, "arity"), &kind),
                0 => r
                    .typed_member(rc, &name)
                    .or_else(|| r.using_member(rc, &name)),
                _ => None,
            }
        } else {
            let Some(contract) = contract else {
                continue;
            };
            let linear = r.linearization(&contract);
            // The extractor already looked in the caller's own contract.
            r.find(&linear[1..], &name, at(rc, "arity"), &kind)
        };
        let Some(target) = target.filter(|t| caller.as_str() != Some(t.as_str())) else {
            continue;
        };
        let relation = if kind == "modifier" { "uses" } else { "calls" };
        if !existing.insert((
            caller.to_string(),
            json!(target).to_string(),
            json!(relation).to_string(),
        )) {
            continue;
        }
        added.push(edge_record(json!({"source":caller,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":at(rc, "source_file"),"source_location":at(rc, "source_location"),"weight":1.0})));
    }
    append_edges(edges, added);
}

fn is_type_meta(metadata: &Value) -> bool {
    metadata["kind"]
        .as_str()
        .is_some_and(|kind| TYPE_KINDS.contains(&kind))
}

/// The indexes [`resolve_solidity_inherited_calls`] binds through.
struct Resolver<'n> {
    node_by_id: HashMap<String, &'n Value>,
    empty: Value,
    owner: HashMap<String, String>,
    functions: HashMap<String, HashMap<String, Vec<(String, Value)>>>,
    modifiers: HashMap<String, HashMap<String, Vec<String>>>,
    bases: HashMap<String, Vec<String>>,
    using: HashMap<String, Vec<Value>>,
    state_variables: HashMap<String, HashMap<String, String>>,
    types_by_file: HashMap<(String, String), Vec<String>>,
    imported: HashMap<String, BTreeSet<String>>,
    file_using: HashMap<String, Vec<Value>>,
    global_using: Vec<(Value, String)>,
    free_functions: HashMap<(String, String), Vec<String>>,
    reachable_cache: RefCell<HashMap<String, Vec<String>>>,
    linear_cache: RefCell<HashMap<String, Vec<String>>>,
}

impl Resolver<'_> {
    /// A node's metadata by id (a JSON value), `Null` when absent.
    fn meta(&self, id: &Value) -> &Value {
        self.node_by_id
            .get(&id.to_string())
            .map(|n| &n["metadata"])
            .filter(|m| m.is_object())
            .unwrap_or(&self.empty)
    }
    fn meta_str(&self, id: &str) -> &Value {
        self.meta(&json!(id))
    }
    fn label(&self, id: &Value) -> String {
        self.node_by_id
            .get(&id.to_string())
            .and_then(|n| n.get("label"))
            .map(python_compat::string)
            .unwrap_or_default()
    }
    fn source_file_of(&self, id: &str) -> String {
        self.node_by_id
            .get(&json!(id).to_string())
            .and_then(|n| n.get("source_file"))
            .map(python_compat::string)
            .unwrap_or_default()
    }
    fn caller_file(&self, caller: &Value) -> String {
        self.node_by_id
            .get(&caller.to_string())
            .and_then(|n| n.get("source_file"))
            .map(python_compat::string)
            .unwrap_or_default()
    }
    fn linearization(&self, type_id: &str) -> Vec<String> {
        self.linear_cache
            .borrow_mut()
            .entry(type_id.to_owned())
            .or_insert_with(|| c3_linearization(type_id, &self.bases))
            .clone()
    }
    fn find(&self, linear: &[String], name: &str, arity: &Value, kind: &str) -> Option<String> {
        for type_id in linear {
            if kind == "member"
                && self
                    .state_variables
                    .get(type_id)
                    .is_some_and(|m| m.contains_key(name))
            {
                // `x.name()` reads a public state variable's getter.
                return None;
            }
            let found: Vec<String> = if kind == "modifier" {
                self.modifiers
                    .get(type_id)
                    .and_then(|m| m.get(name))
                    .cloned()
                    .unwrap_or_default()
            } else {
                self.functions
                    .get(type_id)
                    .and_then(|m| m.get(name))
                    .into_iter()
                    .flatten()
                    .filter(|(id, declared)| {
                        (arity.is_null() || declared.is_null() || declared == arity)
                            // A value's members are its functions, not its
                            // events or errors.
                            && (kind != "member" || self.meta_str(id)["kind"] == "function")
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            if !found.is_empty() {
                return (found.len() == 1).then(|| found[0].clone());
            }
        }
        None
    }
    /// `reachable`: every file `caller_file` imports, directly or through the
    /// files it imports (a re-exported type), sorted.
    fn reachable(&self, caller_file: &str) -> Vec<String> {
        if let Some(done) = self.reachable_cache.borrow().get(caller_file) {
            return done.clone();
        }
        let mut seen = BTreeSet::new();
        let mut stack = self
            .imported
            .get(caller_file)
            .map(|s| s.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        while let Some(current) = stack.pop() {
            if current == caller_file || !seen.insert(current.clone()) {
                continue;
            }
            stack.extend(self.imported.get(&current).into_iter().flatten().cloned());
        }
        let out = seen.into_iter().collect::<Vec<_>>();
        self.reachable_cache
            .borrow_mut()
            .insert(caller_file.to_owned(), out.clone());
        out
    }
    /// `visible_types`: the types named `name` that `caller_file` declares or
    /// imports directly; with `transitive`, when there is none, those of
    /// every file it reaches through its imports.
    fn visible_types(&self, caller_file: &str, name: &str, transitive: bool) -> Vec<String> {
        let by = |file: &str| {
            self.types_by_file
                .get(&(file.to_owned(), name.to_owned()))
                .cloned()
                .unwrap_or_default()
        };
        let mut found = by(caller_file);
        for imported_file in self.imported.get(caller_file).into_iter().flatten() {
            found.extend(by(imported_file));
        }
        if !found.is_empty() || !transitive {
            return found;
        }
        self.reachable(caller_file)
            .iter()
            .flat_map(|f| by(f.as_str()))
            .collect()
    }
    /// `receiver_types`: `(type, file)` readings of a member call's receiver,
    /// best first: the type the extractor read, a base contract's state
    /// variable of the receiver's name, or the declared return type of `g` in
    /// `g(..).f()`. `file` is the file whose scope names the type.
    fn receiver_types(&self, rc: &impl Rec) -> Vec<(String, String)> {
        let caller = at(rc, "caller_nid");
        let mut readings = vec![];
        let caller_file = self.caller_file(caller);
        if crate::build::truthy(at(rc, "receiver_type")) {
            readings.push((python_compat::string(at(rc, "receiver_type")), caller_file));
        }
        let Some(contract) = self.owner.get(&python_compat::string(caller)) else {
            return readings;
        };
        let linear = self.linearization(contract);
        if at(rc, "solidity_call") == "qualified" && crate::build::truthy(at(rc, "qualifier")) {
            // Not a local or own state variable (the extractor checked), so
            // maybe a base contract's.
            let variable = python_compat::string(at(rc, "qualifier"));
            for type_id in &linear[1..] {
                if let Some(nid) = self
                    .state_variables
                    .get(type_id)
                    .and_then(|m| m.get(&variable))
                {
                    let declared = &self.meta_str(nid)["declared_type"];
                    if crate::build::truthy(declared) {
                        readings.push((python_compat::string(declared), self.source_file_of(nid)));
                    }
                    break;
                }
            }
        }
        if let Some([function, count]) = at(rc, "receiver_call").as_array().map(Vec::as_slice)
            && let Some(function) = self.find(
                &linear,
                &python_compat::string(function),
                count,
                "inherited",
            )
        {
            let returned = &self.meta_str(&function)["returns"];
            if crate::build::truthy(returned) {
                readings.push((
                    python_compat::string(returned),
                    self.source_file_of(&function),
                ));
            }
        }
        readings
    }
    /// `typed_member`: `recv.name()` bound through the receiver's contract or
    /// interface type, the first reading of it that names a visible type.
    fn typed_member(&self, rc: &impl Rec, name: &str) -> Option<String> {
        for (type_name, scope_file) in self.receiver_types(rc) {
            let scope = self.visible_types(&scope_file, &type_name, true);
            if scope.is_empty() {
                // Not a type here (`uint256`, or `g(x)` was a call, not a
                // conversion): try the next reading.
                continue;
            }
            // Fail closed when the type is ambiguous or a library (a value
            // never has a library type).
            if scope.len() != 1
                || !matches!(
                    self.meta_str(&scope[0])["kind"].as_str(),
                    Some("contract" | "interface")
                )
            {
                return None;
            }
            return self.find(
                &self.linearization(&scope[0]),
                name,
                at(rc, "arity"),
                "member",
            );
        }
        None
    }
    fn visible_free_functions(&self, scope_file: &str, name: &str) -> Vec<String> {
        let by = |file: &str| {
            self.free_functions
                .get(&(file.to_owned(), name.to_owned()))
                .cloned()
                .unwrap_or_default()
        };
        let mut direct = by(scope_file);
        for imported_file in self.imported.get(scope_file).into_iter().flatten() {
            direct.extend(by(imported_file));
        }
        if !direct.is_empty() {
            return direct;
        }
        self.reachable(scope_file)
            .iter()
            .flat_map(|f| by(f.as_str()))
            .collect()
    }
    /// `attached_functions`: the functions named `name` that `directive`
    /// attaches, read in `scope_file`; `None` when its library is ambiguous.
    fn attached_functions(
        &self,
        directive: &Value,
        scope_file: &str,
        name: &str,
    ) -> Option<Vec<String>> {
        let ids_of = |library: &str, function: &str| {
            self.functions
                .get(library)
                .and_then(|m| m.get(function))
                .into_iter()
                .flatten()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        if let Some(library) = directive.get("library") {
            let libraries = self.visible_types(scope_file, &python_compat::string(library), true);
            if libraries.is_empty() {
                return Some(vec![]); // a library outside the corpus
            }
            if libraries.len() != 1 || self.meta_str(&libraries[0])["kind"] != "library" {
                return None;
            }
            return Some(ids_of(&libraries[0], name));
        }
        let mut attached = vec![];
        for bound in directive["functions"].as_array().into_iter().flatten() {
            let bound = python_compat::string(bound);
            let (qualifier, function) = bound.rsplit_once('.').unwrap_or(("", bound.as_str()));
            if function != name {
                continue;
            }
            if qualifier.is_empty() {
                attached.extend(self.visible_free_functions(scope_file, function));
                continue;
            }
            let libraries = self.visible_types(scope_file, qualifier, true);
            if libraries.len() > 1 {
                return None;
            }
            if let Some(library) = libraries.first() {
                attached.extend(ids_of(library, function));
            }
        }
        Some(attached)
    }
    /// `using_member`: `recv.name()` bound to the one function a `using`
    /// directive in scope attaches to the receiver's type, if there is
    /// exactly one.
    fn using_member(&self, rc: &impl Rec, name: &str) -> Option<String> {
        let caller = at(rc, "caller_nid");
        let caller_file = self.caller_file(caller);
        let contract = self
            .owner
            .get(&python_compat::string(caller))
            .cloned()
            .unwrap_or_default();
        let mut scoped = self
            .using
            .get(&contract)
            .into_iter()
            .flatten()
            .map(|d| (d.clone(), caller_file.clone()))
            .collect::<Vec<_>>();
        scoped.extend(
            self.file_using
                .get(&caller_file)
                .into_iter()
                .flatten()
                .filter(|d| !crate::build::truthy(&d["global"]))
                .map(|d| (d.clone(), caller_file.clone())),
        );
        let arity = at(rc, "arity").as_i64()?;
        for (type_name, _scope_file) in self.receiver_types(rc) {
            let mut directives = scoped.clone();
            directives.extend(
                self.global_using
                    .iter()
                    .filter(|(d, _)| d["for"] == json!(type_name))
                    .cloned(),
            );
            let mut found = BTreeSet::new();
            for (directive, scope_file) in &directives {
                let target = &directive["for"];
                if *target != "*" && *target != json!(type_name) {
                    continue;
                }
                let attached = self.attached_functions(directive, scope_file, name)?;
                found.extend(attached.into_iter().filter(|nid| {
                    let meta = self.meta_str(nid);
                    meta["arity"].as_i64() == Some(arity + 1)
                        && meta["first_param_type"] == json!(type_name)
                }));
            }
            if !found.is_empty() {
                return (found.len() == 1).then(|| found.into_iter().next().unwrap());
            }
        }
        None
    }
}
pub fn extract_solidity(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":format!("Solidity grammar failed to load: {}", io_compat::read_error(&error, path))});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&graphicle_tree_sitter_solidity::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":format!("Solidity grammar failed to load: {error}")});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Solidity grammar failed to load: parse cancelled"});
    };
    let root = tree.root_node();
    let path_text = fsutil::path_string(path);
    let stem = file_stem(path);
    let file_id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let file_name = path_syntax::posix_name(&parts).to_string();
    let mut x = Extractor {
        source: &source,
        path: path_text,
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        seen: HashSet::new(),
        seen_edges: HashSet::new(),
        import_aliases: HashMap::new(),
        contract_contexts: HashMap::new(),
        struct_fields: HashMap::new(),
        member_groups: Groups::new(),
    };
    x.add_node(&file_id, &file_name, root, "file", true, false, None, None);
    for declaration in named(root) {
        if declaration.kind() != "import_directive" {
            continue;
        }
        // `import {A as B} from "./A.sol";` makes `B.f()` call `A.f`; a
        // namespace alias (`import "./x.sol" as X;`) has no import_name
        // before it and is not a type (Graphify 0.9.103).
        let mut imported_name: Option<String> = None;
        let mut cursor = declaration.walk();
        for (index, child) in declaration.children(&mut cursor).enumerate() {
            match declaration.field_name_for_child(index as u32) {
                Some("import_name") => imported_name = Some(x.text(child)),
                Some("alias") => {
                    if let Some(name) = imported_name.take() {
                        let alias = x.text(child);
                        x.import_aliases.insert(alias, name);
                    }
                }
                _ if child.kind() == "," => imported_name = None,
                _ => {}
            }
        }
        let Some(source_node) = declaration.child_by_field_name("source") else {
            continue;
        };
        let relative = unquote(&x.text(source_node));
        if relative.is_empty() || relative.starts_with("http:") || relative.starts_with("https:") {
            continue;
        }
        // `../utils/Errors.sol` names the real file, not `proxy/../utils/...`
        // (Graphify 0.9.102).
        let target = crate::paths::normpath(&fsutil::path_string(
            &path.parent().unwrap_or(Path::new("")).join(&relative),
        ));
        x.add_edge(
            &file_id,
            &make_id([target.as_str()]),
            "imports_from",
            declaration,
            None,
            Some(&target),
        );
    }
    // `contract Pair` / `contract _Pair` in one file would share an id too
    // (Graphify 0.9.104).
    let type_groups = collision_groups(named(root).into_iter().filter_map(|declaration| {
        let kind = type_kind(declaration.kind())?;
        let name = x.text(declaration.child_by_field_name("name")?);
        declaration.child_by_field_name("body")?;
        Some((make_id([stem.as_str(), kind, &name]), name))
    }));
    // Receiver typing (Graphify 0.9.104): a type is kept as its normalized
    // text plus the contract it was written in, so a struct, enum or value
    // type declared in a contract reads as `Contract.Name` everywhere.
    let file_context: TypeContext = (None, BTreeSet::new());
    let field_types = |x: &Extractor<'_>, decl: Node<'_>| {
        let mut fields = HashMap::new();
        for field in decl
            .child_by_field_name("body")
            .map(named)
            .unwrap_or_default()
        {
            if field.kind() == "struct_member"
                && let (Some(t), Some(n)) = (
                    field.child_by_field_name("type"),
                    field.child_by_field_name("name"),
                )
            {
                fields.insert(x.text(n), normalize_type(&x.text(t)));
            }
        }
        fields
    };
    for declaration in named(root) {
        if declaration.kind() == "struct_declaration" {
            if let Some(name_node) = declaration.child_by_field_name("name") {
                let fields = field_types(&x, declaration);
                let name = x.text(name_node);
                x.struct_fields.insert(name, (file_context.clone(), fields));
            }
            continue;
        }
        if type_kind(declaration.kind()).is_none() {
            continue;
        }
        let (Some(name_node), Some(body)) = (
            declaration.child_by_field_name("name"),
            declaration.child_by_field_name("body"),
        ) else {
            continue;
        };
        let owner = x.text(name_node);
        let local_types = named(body)
            .into_iter()
            .filter(|m| {
                matches!(
                    m.kind(),
                    "struct_declaration" | "enum_declaration" | "user_defined_type_definition"
                )
            })
            .filter_map(|m| m.child_by_field_name("name"))
            .map(|n| x.text(n))
            .collect::<BTreeSet<_>>();
        let context: TypeContext = (Some(owner.clone()), local_types);
        x.contract_contexts
            .insert(declaration.id(), context.clone());
        for member in named(body) {
            if member.kind() == "struct_declaration"
                && let Some(member_name) = member.child_by_field_name("name")
            {
                let fields = field_types(&x, member);
                let name = format!("{owner}.{}", x.text(member_name));
                x.struct_fields.insert(name, (context.clone(), fields));
            }
        }
    }
    let file_using = x.using_directives(root, &file_context);
    if !file_using.is_empty() {
        // File-level directives apply to every contract and free function of
        // the file; a `global` one to every use of its type.
        x.nodes[0]["metadata"]["using"] = json!(file_using);
    }
    for declaration in named(root) {
        if let Some(kind) = type_kind(declaration.kind()) {
            x.declaration(declaration, kind, &stem, &file_id, &type_groups);
        }
    }
    x.free_functions(root, &stem, &file_id);
    let Extractor {
        nodes,
        edges,
        raw_calls,
        seen,
        ..
    } = x;
    let edges = edges
        .into_iter()
        .filter(|e| {
            seen.contains(e["source"].as_str().unwrap_or(""))
                && (seen.contains(e["target"].as_str().unwrap_or(""))
                    || e["relation"] == "imports_from")
        })
        .collect::<Vec<_>>();
    json!({"nodes":nodes,"edges":edges,"raw_calls":raw_calls})
}

/// Point `solidity_type:<name>` inheritance and `using` edges at the one type
/// of that name declared in the same file or a file it imports.
pub fn resolve_solidity_type_references(per: &[Value], nodes: &[Value], edges: &mut [Value]) {
    resolve_solidity_type_references_any(per, nodes, edges);
}
/// [`resolve_solidity_type_references`] over the edges in either form (the
/// per-file results are not read).
pub fn resolve_solidity_type_references_any<P: PerFile, E: Rec>(
    _per: &[P],
    nodes: &[Value],
    edges: &mut [E],
) {
    let text = |v: &Value| python_compat::string(v);
    let node_by_id = nodes
        .iter()
        .map(|n| (n["id"].to_string(), n))
        .collect::<HashMap<_, _>>();
    // `{str(source_file): id}` then its inverse, with Python dict semantics.
    let mut file_id_by_source: Vec<(String, Value)> = vec![];
    let mut position: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        if !crate::build::truthy(&n["source_file"]) {
            continue;
        }
        let source = text(&n["source_file"]);
        let parts = path_syntax::posix_parts(&source);
        if n.get("label") != Some(&json!(path_syntax::posix_name(&parts))) {
            continue;
        }
        match position.get(&source) {
            Some(i) => file_id_by_source[*i].1 = n["id"].clone(),
            None => {
                position.insert(source.clone(), file_id_by_source.len());
                file_id_by_source.push((source, n["id"].clone()));
            }
        }
    }
    let mut source_by_file_id: HashMap<String, String> = HashMap::new();
    for (source, id) in &file_id_by_source {
        source_by_file_id.insert(id.to_string(), source.clone());
    }
    let mut imported: HashMap<String, BTreeSet<String>> = HashMap::new();
    for edge in edges
        .iter()
        .filter(|e| at(*e, "relation") == "imports_from")
    {
        if let (Some(from), Some(to)) = (
            source_by_file_id.get(&at(edge, "source").to_string()),
            source_by_file_id.get(&at(edge, "target").to_string()),
        ) {
            imported.entry(from.clone()).or_default().insert(to.clone());
        }
    }
    let mut types: HashMap<(String, String), Vec<Value>> = HashMap::new();
    for n in nodes {
        if !n["metadata"].is_object()
            || !TYPE_KINDS.contains(&n["metadata"]["kind"].as_str().unwrap_or("\u{0}"))
            || !crate::build::truthy(&n["source_file"])
        {
            continue;
        }
        let label = n.get("label").map(text).unwrap_or_default();
        types
            .entry((text(&n["source_file"]), label))
            .or_default()
            .push(n["id"].clone());
    }
    for edge in edges.iter_mut() {
        let Some(name) = at(edge, "context")
            .as_str()
            .and_then(|c| c.strip_prefix("solidity_type:"))
            .map(str::to_string)
        else {
            continue;
        };
        let Some(source_node) = node_by_id
            .get(&at(edge, "source").to_string())
            .filter(|n| crate::build::truthy(&n["source_file"]))
        else {
            continue;
        };
        let source_file = text(&source_node["source_file"]);
        let mut candidates = types
            .get(&(source_file.clone(), name.clone()))
            .cloned()
            .unwrap_or_default();
        for other in imported.get(&source_file).into_iter().flatten() {
            candidates.extend(
                types
                    .get(&(other.clone(), name.clone()))
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        if let [target] = candidates.as_slice() {
            edge.rec_set("target", target.clone());
        }
    }
}
