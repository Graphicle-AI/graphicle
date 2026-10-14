//! Opt-in receiver-aware method-call resolution for Python, TypeScript/JavaScript and Go.
//!
//! The parity pipeline leaves `recv.method(...)` unresolved whenever the engine
//! cannot bind `method` to a same-file label. With
//! `ExtractOptions::receiver_resolution` set, this pass re-reads each source
//! file through the run-scoped parse cache, infers the receiver's type from a
//! narrow set of evidence (constructor call, type annotation, method receiver,
//! imported module alias) and adds a `calls` edge when exactly one method of
//! that type — or of one of its direct bases — matches. Anything else becomes
//! an `unresolved_calls` entry instead of a guess. Default output is untouched.
use super::{
    base::is_language_builtin,
    parse_cache,
    receiver_bindings::{self as rb, Evidence, FileFacts, Receiver, TypeRef},
    resolution::{is_type_like_definition, s},
    resolution_facts,
    resolution_types::go_import_path_for_file,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// Maximum candidate ids recorded on an unresolved entry.
pub const CANDIDATE_LIMIT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Python,
    Js,
    Go,
}
impl Lang {
    fn of(path: &Path) -> Option<Lang> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "py" => Some(Lang::Python),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::Js),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Js => "typescript",
            Lang::Go => "go",
        }
    }
}
fn method_key(label: &str) -> &str {
    label.trim().trim_start_matches('.').trim_end_matches("()")
}
fn dedup(v: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    v.into_iter().filter(|x| seen.insert(x.clone())).collect()
}
/// Every table is lookup-only, so hashing replaces ordered maps.
#[derive(Default)]
struct Index {
    label: FxHashMap<String, String>,
    /// Type-like definitions by label (final node ids, plus per-file labels
    /// that an id collision folded away).
    owners: FxHashMap<String, Vec<String>>,
    owner_file: FxHashMap<String, String>,
    methods: FxHashMap<(String, String), Vec<String>>,
    methods_by_name: FxHashMap<String, Vec<String>>,
    functions_by_name: FxHashMap<String, Vec<String>>,
    bases: FxHashMap<String, Vec<String>>,
    implementers: FxHashMap<String, Vec<String>>,
    file_node: FxHashMap<String, String>,
    python_modules: FxHashMap<String, Vec<String>>,
    python_module_aliases: FxHashMap<(String, String), Vec<String>>,
    python_import_targets: FxHashMap<(String, String, String), BTreeSet<String>>,
    children: FxHashMap<String, Vec<String>>,
    parent: FxHashMap<String, String>,
    imports: FxHashMap<String, Vec<(String, Option<String>)>>,
    calls_at: FxHashMap<(String, String), Vec<String>>,
    return_types: FxHashMap<String, Vec<String>>,
    existing_calls: FxHashSet<(String, String)>,
    go_imports: FxHashMap<String, FxHashMap<String, String>>,
    go_package: FxHashMap<String, String>,
    interfaces: FxHashSet<String>,
}
impl Index {
    fn build(per: &[Value], nodes: &[Value], edges: &[Value], root: &Path) -> Index {
        let mut idx = Index::default();
        let mut ids = FxHashSet::default();
        for n in nodes {
            let id = s(n, "id");
            if id.is_empty() {
                continue;
            }
            ids.insert(id.to_string());
            idx.label.insert(id.into(), s(n, "label").into());
            let sf = s(n, "source_file");
            if !sf.is_empty() {
                idx.owner_file.insert(id.into(), sf.into());
                if Path::new(sf).file_name() == Path::new(s(n, "label")).file_name()
                    && s(n, "source_location") == "L1"
                    && s(n, "file_type") == "code"
                {
                    idx.file_node.entry(sf.into()).or_insert(id.into());
                    if sf.ends_with(".py") {
                        let relative = super::resolution::source_key(sf, root);
                        let module = relative
                            .strip_suffix("/__init__.py")
                            .unwrap_or_else(|| relative.trim_end_matches(".py"));
                        for name in [Some(module), module.strip_prefix("src/")]
                            .into_iter()
                            .flatten()
                        {
                            idx.python_modules
                                .entry(name.replace('/', "."))
                                .or_default()
                                .push(id.into());
                        }
                    }
                }
            }
            if !sf.is_empty() && is_type_like_definition(n) {
                idx.owners
                    .entry(s(n, "label").into())
                    .or_default()
                    .push(id.into());
            }
        }
        for r in per {
            for n in r["nodes"].as_array().into_iter().flatten() {
                let id = s(n, "id");
                if ids.contains(id) && !s(n, "source_file").is_empty() && is_type_like_definition(n)
                {
                    let list = idx.owners.entry(s(n, "label").into()).or_default();
                    if !list.iter().any(|x| x == id) {
                        list.push(id.into());
                    }
                }
            }
            if let Some(gi) = r["go_imports"].as_object() {
                let sf = r["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|n| s(n, "source_file"))
                    .find(|sf| !sf.is_empty())
                    .unwrap_or("");
                if !sf.is_empty() {
                    let table = gi
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect();
                    idx.go_imports.insert(sf.into(), table);
                }
            }
        }
        for e in edges {
            let (src, tgt, rel) = (s(e, "source"), s(e, "target"), s(e, "relation"));
            match rel {
                "method" => {
                    if let Some(label) = idx.label.get(tgt) {
                        let key = method_key(label).to_string();
                        idx.methods
                            .entry((src.into(), key.clone()))
                            .or_default()
                            .push(tgt.into());
                        idx.methods_by_name.entry(key).or_default().push(tgt.into());
                    }
                    idx.parent.entry(tgt.into()).or_insert(src.into());
                }
                "contains" => {
                    idx.children.entry(src.into()).or_default().push(tgt.into());
                    idx.parent.entry(tgt.into()).or_insert(src.into());
                    if let Some(label) = idx.label.get(tgt)
                        && label.ends_with(')')
                        && !label.starts_with('.')
                    {
                        idx.functions_by_name
                            .entry(method_key(label).to_string())
                            .or_default()
                            .push(tgt.into());
                    }
                }
                "inherits" | "extends" => idx.bases.entry(src.into()).or_default().push(tgt.into()),
                "implements" => idx
                    .implementers
                    .entry(tgt.into())
                    .or_default()
                    .push(src.into()),
                "imports" | "imports_from" => {
                    let alias = Some(s(e, "local_alias").to_string()).filter(|a| !a.is_empty());
                    idx.imports
                        .entry(src.into())
                        .or_default()
                        .push((tgt.into(), alias));
                }
                "calls" | "indirect_call" => {
                    idx.existing_calls.insert((src.into(), tgt.into()));
                    if rel == "calls" && !s(e, "source_location").is_empty() {
                        idx.calls_at
                            .entry((src.into(), s(e, "source_location").into()))
                            .or_default()
                            .push(tgt.into());
                    }
                }
                "references" if s(e, "context") == "return_type" => {
                    idx.return_types
                        .entry(src.into())
                        .or_default()
                        .push(tgt.into());
                }
                _ => {}
            }
        }
        for v in idx.owners.values_mut() {
            *v = dedup(std::mem::take(v));
        }
        let files: BTreeSet<String> = idx.owner_file.values().cloned().collect();
        for sf in files {
            if Lang::of(Path::new(&sf)) == Some(Lang::Python)
                && let Some(parsed) = resolution_facts::parse_python_tree(&root.join(&sf))
            {
                // Import edges may retain an external target ID after its
                // placeholder node has been discarded. Read exact, top-level
                // import spellings from the AST instead of guessing from IDs.
                let mut cursor = parsed.tree.root_node().walk();
                for statement in parsed.tree.root_node().named_children(&mut cursor) {
                    if statement.kind() != "import_statement" {
                        continue;
                    }
                    let mut imports = statement.walk();
                    for import in statement.named_children(&mut imports) {
                        let (module, alias, bound_module) = if import.kind() == "aliased_import" {
                            let Some(module) = import.child_by_field_name("name") else {
                                continue;
                            };
                            let Some(alias) = import.child_by_field_name("alias") else {
                                continue;
                            };
                            let module = super::base::read_text(module, &parsed.source);
                            let alias = super::base::read_text(alias, &parsed.source);
                            (module.clone(), alias, module)
                        } else if import.kind() == "dotted_name" {
                            // `import pkg.sub` binds pkg; `import pkg.sub as p`
                            // binds the complete submodule to p.
                            let name = super::base::read_text(import, &parsed.source);
                            let first = name.split('.').next().unwrap_or("").to_string();
                            (name, first.clone(), first)
                        } else {
                            continue;
                        };
                        idx.python_module_aliases
                            .entry((sf.clone(), alias))
                            .or_default()
                            .push(bound_module);
                        if let Some(targets) = idx.python_modules.get(&module)
                            && let Some(file) = idx.file_node.get(&sf)
                        {
                            idx.python_import_targets
                                .entry((
                                    file.clone(),
                                    super::base::make_id([module.as_str()]),
                                    format!("L{}", statement.start_position().row + 1),
                                ))
                                .or_default()
                                .extend(targets.iter().cloned());
                        }
                    }
                }
            }
            if Lang::of(Path::new(&sf)) == Some(Lang::Go)
                && let Some(pkg) = go_import_path_for_file(Path::new(&sf), root)
            {
                idx.go_package.insert(sf, pkg);
            }
        }
        idx
    }
    fn file_id(&self, sf: &str) -> Option<&String> {
        self.file_node.get(sf)
    }
    /// The module node an import alias `name` refers to in file `sf`, if unique.
    fn module_alias(&self, sf: &str, name: &str) -> Option<String> {
        let file = self.file_id(sf)?;
        let mut hits = BTreeSet::new();
        if let Some(imports) = self.python_module_aliases.get(&(sf.into(), name.into())) {
            // Rebinding to another (possibly external) module must not retain
            // the one local candidate merely because it has graph nodes.
            let modules = imports.iter().collect::<BTreeSet<_>>();
            if modules.len() != 1 {
                return None;
            }
            let ids = self.python_modules.get(*modules.first()?)?;
            let ids = ids.iter().collect::<BTreeSet<_>>();
            return (ids.len() == 1).then(|| (*ids.first().unwrap()).clone());
        }
        for (target, alias) in self.imports.get(file).into_iter().flatten() {
            // The parity extractor leaves `import package` as an external
            // placeholder even when package/__init__.py is in this corpus.
            // Resolve the exact package path; never a global function name.
            if Lang::of(Path::new(sf)) == Some(Lang::Python)
                && let Some(imported) = self.label.get(target)
                && (alias.as_deref() == Some(name) || (alias.is_none() && imported == name))
                && let Some(modules) = self.python_modules.get(imported)
            {
                hits.extend(modules.iter().cloned());
            }
            if !self.children.contains_key(target) && !self.imports.contains_key(target) {
                continue;
            }
            let stem = self
                .owner_file
                .get(target)
                .map(|f| {
                    let path = Path::new(f);
                    let component = if path.file_name().is_some_and(|n| n == "__init__.py") {
                        path.parent().and_then(Path::file_name)
                    } else {
                        path.file_stem()
                    };
                    component.unwrap_or_default().to_string_lossy().into_owned()
                })
                .unwrap_or_default();
            if alias.as_deref() == Some(name)
                || (alias.is_none()
                    && (stem == name || self.label.get(target).is_some_and(|l| l == name)))
            {
                hits.insert(target.clone());
            }
        }
        (hits.len() == 1).then(|| hits.into_iter().next().unwrap())
    }
    fn module_functions(&self, module: &str, callee: &str, lang: Lang) -> Vec<String> {
        let mut found = self
            .children
            .get(module)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.label
                    .get(*id)
                    .is_some_and(|l| l.ends_with(')') && method_key(l) == callee)
            })
            .cloned()
            .collect::<Vec<_>>();
        if lang == Lang::Python {
            // Python module attributes include functions imported into that
            // module, including __init__ re-exports and explicit aliases.
            for (target, alias) in self.imports.get(module).into_iter().flatten() {
                if self.label.get(target).is_some_and(|l| {
                    l.ends_with(')') && alias.as_deref().unwrap_or(method_key(l)) == callee
                }) {
                    found.push(target.clone());
                }
            }
        }
        dedup(found)
    }
    fn imported_set(&self, sf: &str) -> BTreeSet<&String> {
        self.file_id(sf)
            .and_then(|f| self.imports.get(f))
            .into_iter()
            .flatten()
            .map(|(t, _)| t)
            .collect()
    }
    /// Go declares a receiver node per file that defines methods on the type;
    /// all same-named nodes of one package stand for a single type.
    fn go_group(&self, ids: Vec<String>) -> Vec<String> {
        let mut out = ids.clone();
        for id in &ids {
            let (Some(label), Some(file)) = (self.label.get(id), self.owner_file.get(id)) else {
                continue;
            };
            let dir = Path::new(file).parent();
            for other in self.owners.get(label).into_iter().flatten() {
                if self
                    .owner_file
                    .get(other)
                    .is_some_and(|f| Path::new(f).parent() == dir)
                {
                    out.push(other.clone());
                }
            }
        }
        dedup(out)
    }
    fn same_package(&self, ids: &[String]) -> bool {
        let dirs: BTreeSet<_> = ids
            .iter()
            .filter_map(|id| self.owner_file.get(id))
            .map(|f| Path::new(f).parent())
            .collect();
        dirs.len() == 1
    }
    /// Whether `func(...)` at `line` inside `caller` was bound to a repository function.
    fn corpus_call(&self, caller: &str, func: &str, line: usize) -> bool {
        let name = func.rsplit('.').next().unwrap_or(func);
        let wanted = format!("{name}()");
        self.calls_at
            .get(&(caller.into(), format!("L{line}")))
            .into_iter()
            .flatten()
            .any(|t| self.label.get(t).is_some_and(|l| *l == wanted))
    }
    fn owners_for(&self, lang: Lang, sf: &str, caller: &str, ty: &TypeRef) -> Vec<String> {
        match ty {
            TypeRef::Return(func, line) => {
                let name = func.rsplit('.').next().unwrap_or(func);
                let wanted = format!("{name}()");
                let loc = format!("L{line}");
                let mut out = vec![];
                for target in self
                    .calls_at
                    .get(&(caller.into(), loc))
                    .into_iter()
                    .flatten()
                {
                    if self.label.get(target).is_some_and(|l| *l == wanted) {
                        for t in self.return_types.get(target).into_iter().flatten() {
                            if self.owner_file.contains_key(t)
                                && self.owners.values().any(|v| v.contains(t))
                            {
                                out.push(t.clone());
                            }
                        }
                    }
                }
                dedup(out)
            }
            TypeRef::Name(raw) => {
                let raw = raw.trim_start_matches('*');
                let (qual, name) = match raw.rsplit_once('.') {
                    Some((q, n)) => (Some(q), n),
                    None => (None, raw),
                };
                let Some(all) = self.owners.get(name) else {
                    return vec![];
                };
                let file_of =
                    |id: &String| self.owner_file.get(id).map(String::as_str).unwrap_or("");
                if let Some(q) = qual {
                    let narrowed: Vec<String> = match lang {
                        Lang::Go => {
                            let pkg = self.go_imports.get(sf).and_then(|t| t.get(q));
                            all.iter()
                                .filter(|id| {
                                    pkg.is_some_and(|p| self.go_package.get(file_of(id)) == Some(p))
                                })
                                .cloned()
                                .collect()
                        }
                        _ => {
                            let module = self.module_alias(sf, q);
                            all.iter()
                                .filter(|id| {
                                    module
                                        .as_ref()
                                        .is_some_and(|m| self.parent.get(*id) == Some(m))
                                })
                                .cloned()
                                .collect()
                        }
                    };
                    if !narrowed.is_empty() {
                        return narrowed;
                    }
                }
                let same_file: Vec<String> =
                    all.iter().filter(|id| file_of(id) == sf).cloned().collect();
                if !same_file.is_empty() {
                    return same_file;
                }
                if lang == Lang::Go && qual.is_none() {
                    let dir = Path::new(sf).parent();
                    let same_pkg: Vec<String> = all
                        .iter()
                        .filter(|id| Path::new(file_of(id)).parent() == dir)
                        .cloned()
                        .collect();
                    if !same_pkg.is_empty() {
                        return same_pkg;
                    }
                }
                let imported = self.imported_set(sf);
                let via_import: Vec<String> = all
                    .iter()
                    .filter(|id| {
                        imported.contains(id)
                            || self.parent.get(*id).is_some_and(|p| imported.contains(p))
                    })
                    .cloned()
                    .collect();
                if !via_import.is_empty() {
                    return via_import;
                }
                all.clone()
            }
        }
    }
    fn methods_of(&self, owner: &str, callee: &str) -> Vec<String> {
        self.methods
            .get(&(owner.into(), callee.into()))
            .cloned()
            .unwrap_or_default()
    }
    fn global(&self, callee: &str) -> Vec<String> {
        let mut v = self
            .methods_by_name
            .get(callee)
            .cloned()
            .unwrap_or_default();
        v.truncate(CANDIDATE_LIMIT);
        v
    }
}
enum Outcome {
    Edge {
        target: String,
        evidence: Evidence,
        receiver_type: Option<String>,
    },
    Unresolved {
        reason: &'static str,
        candidates: Vec<String>,
        receiver_type: Option<String>,
    },
    Skip,
}
fn unresolved(
    reason: &'static str,
    candidates: Vec<String>,
    receiver_type: Option<String>,
) -> Outcome {
    let mut candidates = dedup(candidates);
    candidates.truncate(CANDIDATE_LIMIT);
    Outcome::Unresolved {
        reason,
        candidates,
        receiver_type,
    }
}
/// The binding that fixes `name` at `byte`: innermost scope that binds the
/// name at all, and within it the latest binding before the call.
fn lookup(facts: &FileFacts, byte: usize, name: &str) -> Option<Option<(TypeRef, Evidence)>> {
    for scope in facts.enclosing(byte) {
        let visible = scope
            .bindings
            .iter()
            .filter(|b| b.name == name && (b.whole_scope || b.at < byte))
            .max_by_key(|b| b.at);
        if let Some(b) = visible {
            return Some(b.ty.clone().map(|t| (t, b.evidence)));
        }
    }
    None
}
fn field_type(
    facts: &FileFacts,
    byte: usize,
    base: &str,
    field: &str,
) -> Option<(TypeRef, Evidence)> {
    let class = facts
        .enclosing(byte)
        .into_iter()
        .find(|s| s.self_names.iter().any(|n| n == base))
        .and_then(|s| s.class.clone())?;
    let bindings: Vec<_> = facts
        .fields
        .get(&class)
        .into_iter()
        .flatten()
        .filter(|b| b.name == field)
        .collect();
    if bindings.is_empty() {
        return None;
    }
    let mut types: Vec<(TypeRef, Evidence)> = vec![];
    for b in &bindings {
        let t = b.ty.clone()?;
        if !types.iter().any(|(x, _)| *x == t) {
            types.push((t, b.evidence));
        }
    }
    if types.len() == 1 {
        // Prefer the annotation label when both an annotation and a constructor agree.
        Some(types.pop().unwrap())
    } else {
        None
    }
}
/// Method names that are overwhelmingly standard-library calls (`s.toUpperCase()`,
/// `items.append(x)`). Only used to keep them out of `unresolved_calls` when the
/// receiver type is unknown; they never influence resolution.
fn is_std_method(name: &str) -> bool {
    const STD: &[&str] = &[
        "append",
        "capitalize",
        "casefold",
        "catch",
        "charAt",
        "charCodeAt",
        "concat",
        "decode",
        "discard",
        "encode",
        "endsWith",
        "endswith",
        "entries",
        "every",
        "extend",
        "filter",
        "finally",
        "findIndex",
        "flat",
        "flatMap",
        "forEach",
        "hasOwnProperty",
        "includes",
        "indexOf",
        "isalnum",
        "isalpha",
        "isdigit",
        "isspace",
        "items",
        "join",
        "keys",
        "lastIndexOf",
        "localeCompare",
        "lower",
        "lstrip",
        "map",
        "padEnd",
        "padStart",
        "pop",
        "popitem",
        "push",
        "reduce",
        "repeat",
        "replace",
        "replaceAll",
        "rsplit",
        "rstrip",
        "setdefault",
        "shift",
        "slice",
        "some",
        "splice",
        "split",
        "splitlines",
        "startsWith",
        "startswith",
        "strip",
        "substr",
        "substring",
        "then",
        "title",
        "toFixed",
        "toISOString",
        "toLowerCase",
        "toString",
        "toUpperCase",
        "trim",
        "trimEnd",
        "trimStart",
        "unshift",
        "upper",
        "values",
        "zfill",
    ];
    STD.binary_search(&name).is_ok()
}
fn is_literal_receiver(text: &str) -> bool {
    let t = text.trim_start();
    let quoted = t
        .find(['"', '\'', '`'])
        .is_some_and(|i| i <= 2 && t[..i].chars().all(|c| "bfruBFRU".contains(c)));
    quoted || t.starts_with(['[', '{', '(']) || t.starts_with(|c: char| c.is_ascii_digit())
}
fn resolve_one(
    idx: &Index,
    lang: Lang,
    facts: &FileFacts,
    site: &rb::CallSite,
    caller: &str,
    sf: &str,
) -> Outcome {
    let callee = site.callee.as_str();
    let inferred: Option<(TypeRef, Evidence)> = match &site.receiver {
        Receiver::Name(name) => {
            let bound = lookup(facts, site.byte, name);
            if bound.is_none()
                && let Some(module) = idx.module_alias(sf, name)
            {
                let hits = idx.module_functions(&module, callee, lang);
                return match hits.len() {
                    1 => Outcome::Edge {
                        target: hits[0].clone(),
                        evidence: Evidence::ModuleAttr,
                        receiver_type: idx.label.get(&module).cloned(),
                    },
                    0 => {
                        let global = idx
                            .functions_by_name
                            .get(callee)
                            .cloned()
                            .unwrap_or_default();
                        if global.is_empty() {
                            Outcome::Skip
                        } else {
                            unresolved("no_definition", global, idx.label.get(&module).cloned())
                        }
                    }
                    _ => unresolved("ambiguous", hits, idx.label.get(&module).cloned()),
                };
            }
            bound.flatten()
        }
        Receiver::Field { base, field } => field_type(facts, site.byte, base, field),
        // Literal receivers (`", ".join(...)`, `[].push(...)`) are never corpus types.
        Receiver::Other(text) if is_literal_receiver(text) => return Outcome::Skip,
        Receiver::Other(_) => None,
    };
    let Some((ty, evidence)) = inferred else {
        if is_language_builtin(callee) || is_std_method(callee) {
            return Outcome::Skip;
        }
        let global = idx.global(callee);
        return if global.is_empty() {
            Outcome::Skip
        } else {
            unresolved("unknown_receiver", global, None)
        };
    };
    let type_label = match &ty {
        TypeRef::Name(n) => Some(n.trim_start_matches('*').to_string()),
        TypeRef::Return(f, _) => Some(format!("{f}()")),
    };
    let mut owners = idx.owners_for(lang, sf, caller, &ty);
    if lang == Lang::Go {
        owners = idx.go_group(owners);
    }
    if owners.is_empty() {
        // The receiver's type is known but not defined in the corpus (external
        // library class, `dict()`, `json.loads(...)`): not a repository call, so
        // it is skipped silently. A repository factory whose return type is
        // unknown is still worth listing.
        let external = match &ty {
            TypeRef::Name(_) => true,
            TypeRef::Return(func, line) => !idx.corpus_call(caller, func, *line),
        };
        let global = idx.global(callee);
        return if external || global.is_empty() || is_language_builtin(callee) {
            Outcome::Skip
        } else {
            unresolved("unknown_receiver", global, type_label)
        };
    }
    if owners.len() > 1 && !(lang == Lang::Go && idx.same_package(&owners)) {
        let cands: Vec<String> = owners
            .iter()
            .flat_map(|o| idx.methods_of(o, callee))
            .collect();
        return unresolved(
            "ambiguous",
            if cands.is_empty() { owners } else { cands },
            type_label,
        );
    }
    let receiver_type = idx.label.get(&owners[0]).cloned().or(type_label);
    if owners.iter().any(|o| idx.interfaces.contains(o)) {
        let mut cands: Vec<String> = owners
            .iter()
            .flat_map(|o| idx.implementers.get(o).into_iter().flatten())
            .flat_map(|c| idx.methods_of(c, callee))
            .collect();
        if cands.is_empty() {
            cands = idx.global(callee);
        }
        return unresolved("ambiguous", cands, receiver_type);
    }
    let direct = dedup(
        owners
            .iter()
            .flat_map(|o| idx.methods_of(o, callee))
            .collect(),
    );
    match direct.len() {
        1 => {
            return Outcome::Edge {
                target: direct[0].clone(),
                evidence,
                receiver_type,
            };
        }
        0 => {}
        _ => return unresolved("ambiguous", direct, receiver_type),
    }
    let inherited = dedup(
        owners
            .iter()
            .flat_map(|o| idx.bases.get(o).into_iter().flatten())
            .flat_map(|b| idx.methods_of(b, callee))
            .collect(),
    );
    match inherited.len() {
        1 => Outcome::Edge {
            target: inherited[0].clone(),
            evidence,
            receiver_type,
        },
        0 => unresolved("no_definition", idx.global(callee), receiver_type),
        _ => unresolved("ambiguous", inherited, receiver_type),
    }
}
fn parse(path: &Path, lang: Lang) -> Option<parse_cache::Tree> {
    match lang {
        Lang::Python => resolution_facts::parse_python_tree(path),
        Lang::Js => resolution_facts::parse_js_tree(path),
        Lang::Go => parse_cache::parse(path, "go", tree_sitter_go::LANGUAGE.into()),
    }
}
fn facts_for(path: &Path, lang: Lang) -> Option<FileFacts> {
    let tree = parse(path, lang)?;
    let root = tree.tree.root_node();
    Some(match lang {
        Lang::Python => rb::python_facts(root, &tree.source),
        Lang::Js => rb::js_facts(root, &tree.source),
        Lang::Go => rb::go_facts(root, &tree.source),
    })
}
fn line_of(loc: &str) -> Option<usize> {
    loc.trim_start_matches('L')
        .split([':', '-'])
        .next()?
        .parse()
        .ok()
}
/// Add receiver-resolved `calls` edges; returns the `unresolved_calls` entries.
pub fn resolve(
    paths: &[PathBuf],
    per: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
    root: &Path,
) -> Vec<Value> {
    let mut idx = Index::build(per, nodes, edges, root);
    // The same exact import proof also connects the package file for wider
    // impact analysis. Keep already bound repository targets unchanged.
    // This pass is opt-in, so the pinned default extraction stays identical.
    for edge in edges.iter_mut() {
        if s(edge, "relation") == "imports"
            && !idx.owner_file.contains_key(s(edge, "target"))
            && let Some(targets) = idx.python_import_targets.get(&(
                s(edge, "source").into(),
                s(edge, "target").into(),
                s(edge, "source_location").into(),
            ))
            && targets.len() == 1
            && let Some(target) = targets.first()
        {
            edge["target"] = json!(target);
            edge["confidence"] = json!("INFERRED");
            edge["confidence_score"] = json!(0.85);
            edge["derivation"] = json!("inferred");
            edge["resolution"] = json!("python_module_import");
        }
    }
    let mut by_file: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for rc in per
        .iter()
        .flat_map(|r| r["raw_calls"].as_array().into_iter().flatten())
    {
        let sf = s(rc, "source_file");
        if rc["is_member_call"] == true
            && !s(rc, "callee").is_empty()
            && !s(rc, "caller_nid").is_empty()
            && Lang::of(Path::new(sf)).is_some()
        {
            by_file.entry(sf.into()).or_default().push(rc);
        }
    }
    // Interface declarations must be known for every file, not only those
    // with unresolved member calls.
    let mut facts_cache: BTreeMap<String, Option<FileFacts>> = BTreeMap::new();
    let mut wanted: BTreeSet<String> = by_file.keys().cloned().collect();
    for p in paths {
        if matches!(Lang::of(p), Some(Lang::Js | Lang::Go)) {
            wanted.insert(p.to_string_lossy().into_owned());
        }
    }
    // Per-file facts depend only on the file's own tree, so gather them in
    // parallel and fold the interface names in the original (sorted) order.
    let wanted = wanted.into_iter().collect::<Vec<_>>();
    let gathered = parse_cache::par_map(&wanted, |sf| {
        let path = Path::new(sf);
        let lang = Lang::of(path)?;
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        Some(facts_for(&abs, lang))
    });
    for (sf, facts) in wanted.iter().zip(gathered) {
        let Some(facts) = facts else { continue };
        if let Some(f) = &facts {
            for name in &f.interfaces {
                for id in idx.owners.get(name).into_iter().flatten() {
                    if idx.owner_file.get(id).is_some_and(|f| f == sf) {
                        idx.interfaces.insert(id.clone());
                    }
                }
            }
        }
        facts_cache.insert(sf.clone(), facts);
    }
    let mut out: Vec<Value> = vec![];
    // One `unresolved_calls` entry per (caller, callee, receiver); repeats add to `occurrences`.
    let mut unresolved_at: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for (sf, records) in &by_file {
        let lang = Lang::of(Path::new(sf)).unwrap();
        let Some(Some(facts)) = facts_cache.get(sf) else {
            continue;
        };
        let mut sites: BTreeMap<(usize, &str), VecDeque<&rb::CallSite>> = BTreeMap::new();
        for site in &facts.calls {
            sites
                .entry((site.line, site.callee.as_str()))
                .or_default()
                .push_back(site);
        }
        for rc in records {
            let (caller, callee, loc) = (
                s(rc, "caller_nid"),
                s(rc, "callee"),
                s(rc, "source_location"),
            );
            let Some(line) = line_of(loc) else { continue };
            let Some(site) = sites.get_mut(&(line, callee)).and_then(VecDeque::pop_front) else {
                continue;
            };
            match resolve_one(&idx, lang, facts, site, caller, sf) {
                Outcome::Edge {
                    target,
                    evidence,
                    receiver_type,
                } => {
                    if target == caller
                        || !idx.existing_calls.insert((caller.into(), target.clone()))
                    {
                        continue;
                    }
                    let mut e = json!({
                        "source": caller,
                        "target": target,
                        "relation": "calls",
                        "context": "call",
                        "confidence": "INFERRED",
                        "confidence_score": 0.85,
                        "derivation": "inferred",
                        "receiver_evidence": evidence.as_str(),
                        "source_file": sf,
                        "source_location": loc,
                        "weight": 1.0,
                    });
                    if let Some(t) = receiver_type {
                        e["receiver_type"] = json!(t);
                    }
                    edges.push(e);
                }
                Outcome::Unresolved {
                    reason,
                    candidates,
                    receiver_type,
                } => {
                    let receiver = site.receiver.text();
                    let key = (caller.to_string(), callee.to_string(), receiver.clone());
                    if let Some(&i) = unresolved_at.get(&key) {
                        let n = out[i]["occurrences"].as_u64().unwrap_or(1) + 1;
                        out[i]["occurrences"] = json!(n);
                        continue;
                    }
                    let mut u = json!({
                        "caller": caller,
                        "callee": callee,
                        "receiver": receiver,
                        "reason": reason,
                        "candidates": candidates,
                        "language": lang.name(),
                        "source_file": sf,
                        "source_location": loc,
                        "occurrences": 1,
                    });
                    if let Some(t) = receiver_type {
                        u["receiver_type"] = json!(t);
                    }
                    unresolved_at.insert(key, out.len());
                    out.push(u);
                }
                Outcome::Skip => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::extract::{ExtractOptions, extract};
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }
    fn run(root: &Path, paths: &[PathBuf], on: bool) -> Value {
        // `extract` opens a process-wide parse-cache run; keep clear of the cache tests.
        let _serial = crate::extractors::parse_cache::test_serial();
        extract(
            paths,
            &ExtractOptions {
                root: Some(root.to_path_buf()),
                parallel: false,
                use_cache: false,
                receiver_resolution: on,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn calls(v: &Value) -> Vec<(String, String, String)> {
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls" && e.get("receiver_evidence").is_some())
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                    e["receiver_evidence"].as_str().unwrap().into(),
                )
            })
            .collect()
    }
    fn unresolved(v: &Value) -> Vec<&Value> {
        v["unresolved_calls"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }
    fn count_calls(v: &Value, src: &str, tgt: &str) -> usize {
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls" && e["source"] == src && e["target"] == tgt)
            .count()
    }
    fn has(calls: &[(String, String, String)], src: &str, tgt: &str, ev: &str) -> bool {
        calls
            .iter()
            .any(|(s, t, e)| s == src && t == tgt && e == ev)
    }

    #[test]
    fn python_package_reexports_are_scoped_and_shadowed_receivers_stay_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let root = canonical.as_path();
        let paths = vec![
            write(root, "parcel/__init__.py", "from .api import post\n"),
            write(root, "parcel/api.py", "def post():\n    return 1\n"),
            write(root, "other.py", "def post():\n    return 2\n"),
            write(
                root,
                "tests/test_api.py",
                "import parcel\nimport parcel as parcels\nimport parcel as replaced\nimport unavailable as replaced\ndef test_post():\n    return parcel.post()\ndef test_alias():\n    return parcels.post()\ndef shadowed(parcel):\n    return parcel.post()\ndef rebound():\n    return replaced.post()\n",
            ),
        ];
        let graph = run(root, &paths, true);
        let edges = calls(&graph);
        // Since upstream #3729 the extractor itself binds `import parcel` to
        // the package file, so this pass has no module import left to repoint.
        assert!(graph["edges"].as_array().unwrap().iter().any(|e| {
            e["source"] == "tests_test_api"
                && e["target"] == "parcel_init"
                && e["relation"] == "imports"
                && e["confidence"] == "EXTRACTED"
        }));
        assert!(
            has(
                &edges,
                "tests_test_api_test_post",
                "parcel_api_post",
                "module_attr"
            ),
            "{graph}"
        );
        assert!(has(
            &edges,
            "tests_test_api_test_alias",
            "parcel_api_post",
            "module_attr"
        ));
        assert!(
            !edges
                .iter()
                .any(|(s, t, _)| s == "tests_test_api_shadowed" && t == "parcel_api_post")
        );
        assert!(
            !edges
                .iter()
                .any(|(s, t, _)| s == "tests_test_api_rebound" && t == "parcel_api_post")
        );
        // The old default port boundary remains unchanged.
        assert!(!has(
            &calls(&run(root, &paths, false)),
            "tests_test_api_test_post",
            "parcel_api_post",
            "module_attr"
        ));
    }

    #[test]
    fn python_receiver_evidence_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = write(
            root,
            "app.py",
            r#"import helper
from base import Base

class Client(Base):
    def send(self, data):
        return self.ping() + self.fmt(data)
    def fmt(self, d):
        return d

class Other:
    def send(self, x):
        return x

def use(c: Client):
    return c.send(1)

def main():
    client = Client("x")
    client.send(2)
    duck.send(3)
    helper.build()
"#,
        );
        let b = write(
            root,
            "base.py",
            "class Base:\n    def ping(self):\n        return 1\n",
        );
        let h = write(root, "helper.py", "def build():\n    return 1\n");
        let paths = vec![a, b, h];
        let off = run(root, &paths, false);
        assert!(calls(&off).is_empty(), "default mode must add nothing");
        assert!(off.get("unresolved_calls").is_none());
        let on = run(root, &paths, true);
        let c = calls(&on);
        // `c.send()` on an annotated parameter and `client.send()` on a
        // constructed local carry `receiver_type` and are bound by the parity
        // typed-receiver pass (Graphify 0.9.88, as the reference binds them);
        // the opt-in pass must not duplicate them.
        for (caller, mode) in [("app_use", "annotation"), ("app_main", "constructor")] {
            assert_eq!(count_calls(&off, caller, "app_client_send"), 1, "{caller}");
            assert_eq!(count_calls(&on, caller, "app_client_send"), 1, "{caller}");
            assert!(!has(&c, caller, "app_client_send", mode), "{c:?}");
        }
        // `self.ping()` on the imported base in another file is bound by the
        // parity `python_members` pass (Graphify 0.9.81, #4222); the opt-in
        // pass must not duplicate it.
        assert_eq!(count_calls(&off, "app_client_send", "base_base_ping"), 1);
        assert_eq!(count_calls(&on, "app_client_send", "base_base_ping"), 1);
        // `module.func()` is already bound by the parity `python_members` pass;
        // the opt-in pass must not duplicate it.
        assert_eq!(count_calls(&on, "app_main", "helper_build"), 1);
        let u = unresolved(&on);
        let duck = u
            .iter()
            .find(|u| u["receiver"] == "duck")
            .expect("duck-typed receiver is unresolved");
        assert_eq!(duck["reason"], "unknown_receiver");
        assert_eq!(duck["candidates"].as_array().unwrap().len(), 2, "{duck}");
        for e in on["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.get("receiver_evidence").is_some())
        {
            assert_eq!(e["confidence"], "INFERRED");
            assert_eq!(e["derivation"], "inferred");
            assert!(e["source_location"].as_str().unwrap().starts_with('L'));
        }
    }

    #[test]
    fn python_ambiguous_and_missing_targets_are_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = write(
            root,
            "a.py",
            "class Client:\n    def send(self):\n        return 1\n",
        );
        let b = write(
            root,
            "b.py",
            "class Client:\n    def send(self):\n        return 2\n",
        );
        let m = write(
            root,
            "m.py",
            r#"def run(c: Client):
    c.send()

def other():
    x = Widget()
    x.paint()
def duck(x):
    x.send()
"#,
        );
        let on = run(root, &[a, b, m], true);
        assert!(calls(&on).is_empty(), "{:?}", calls(&on));
        let u = unresolved(&on);
        let amb = u
            .iter()
            .find(|u| u["callee"] == "send" && u["caller"] == "m_run")
            .unwrap();
        assert_eq!(amb["reason"], "ambiguous");
        assert_eq!(amb["candidates"].as_array().unwrap().len(), 2);
        let duck = u
            .iter()
            .find(|u| u["callee"] == "send" && u["caller"] == "m_duck")
            .expect("duck-typed receiver is reported, not guessed");
        assert_eq!(duck["reason"], "unknown_receiver");
        assert_eq!(duck["candidates"].as_array().unwrap().len(), 2);
        assert!(
            u.iter().all(|u| u["callee"] != "paint"),
            "external types are skipped silently"
        );
    }

    #[test]
    fn typescript_receiver_evidence_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = write(
            root,
            "app.ts",
            r#"import { Base } from './base';
import { Client } from './client';
import { Sender } from './types';
import * as util from './util';
class Wrapper extends Base {
  private inner: Client;
  constructor() { super(); this.inner = new Client(); }
  run() { this.inner.send(1); return this.ping(); }
}
function use(c: Client, s: Sender) { c.send(1); s.send(2); util.helper(); }
function main() { const client = new Client(); client.send(2); }
function other() { const c = new Base(); c.ping(); }
"#,
        );
        let b = write(
            root,
            "base.ts",
            "export class Base { ping() { return 1; } }\n",
        );
        let c = write(
            root,
            "client.ts",
            "export class Client { send(x: number) { return x; } }\n",
        );
        let t = write(
            root,
            "types.ts",
            "export interface Sender { send(x: number): void; }\nexport class Other implements Sender { send(x: number) {} }\n",
        );
        let u = write(root, "util.ts", "export function helper() { return 1; }\n");
        let paths = vec![a, b, c, t, u];
        assert!(calls(&run(root, &paths, false)).is_empty());
        let on = run(root, &paths, true);
        let c = calls(&on);
        // `c` has conflicting file-level types (Client in `use`, Base in
        // `other`), so the parity file-level table drops it; the scoped
        // annotation resolves it.
        assert!(
            has(&c, "app_use", "client_client_send", "annotation"),
            "{c:?}"
        );
        // `client.send(2)` is already bound by the parity file-level type
        // table; the opt-in pass must not duplicate it.
        let n = count_calls(&on, "app_main", "client_client_send");
        assert_eq!(n, 1);
        assert!(
            has(&c, "app_wrapper_run", "base_base_ping", "self_method"),
            "{c:?}"
        );
        // The field annotation `inner: Client` is in the parity type table
        // since Graphify 0.9.81 (#4231), which binds `this.inner.send(1)`; the
        // opt-in pass must not duplicate it.
        assert_eq!(count_calls(&on, "app_wrapper_run", "client_client_send"), 1);
        assert!(has(&c, "app_use", "util_helper", "module_attr"), "{c:?}");
        assert!(
            !c.iter().any(|(_, t, _)| t == "types_sender_send"),
            "interface methods are never targets"
        );
        let u = unresolved(&on);
        let iface = u
            .iter()
            .find(|u| u["receiver"] == "s")
            .expect("interface-typed receiver stays unresolved");
        assert_eq!(iface["reason"], "ambiguous");
        assert_eq!(iface["receiver_type"], "Sender");
        assert_eq!(iface["candidates"], serde_json::json!(["types_other_send"]));
    }

    #[test]
    fn go_receiver_evidence_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "go.mod", "module example.com/m\n");
        let a = write(
            root,
            "pkg/conn.go",
            r#"package pkg

type Sender interface { Send(x int) int }

type Conn struct{ url string }

func NewConn(url string) *Conn { return &Conn{url: url} }

func (c *Conn) Send(x int) int { return c.ping() + x }
func (c *Conn) ping() int { return 1 }

type Other struct{}

func (o Other) Send(x int) int { return x }
"#,
        );
        let b = write(
            root,
            "cmd/main.go",
            r#"package main

import "example.com/m/pkg"

func use(c *pkg.Conn, s pkg.Sender) { c.Send(1); s.Send(2) }

func main() {
	conn := pkg.NewConn("x")
	conn.Send(2)
	o := &pkg.Other{}
	o.Send(3)
	var d pkg.Conn
	d.Send(4)
}
"#,
        );
        let paths = vec![a, b];
        assert!(calls(&run(root, &paths, false)).is_empty());
        let on = run(root, &paths, true);
        let c = calls(&on);
        assert!(
            has(&c, "cmd_main_use", "pkg_conn_send", "annotation"),
            "{c:?}"
        );
        assert!(
            has(&c, "cmd_main_main", "pkg_conn_send", "constructor"),
            "{c:?}"
        );
        assert!(
            has(&c, "cmd_main_main", "pkg_other_send", "constructor"),
            "{c:?}"
        );
        let u = unresolved(&on);
        let iface = u
            .iter()
            .find(|u| u["receiver"] == "s")
            .expect("interface receiver stays unresolved");
        assert_eq!(iface["reason"], "ambiguous");
        assert!(!iface["candidates"].as_array().unwrap().is_empty());
    }

    #[test]
    fn go_typed_receiver_resolves_inherited_and_same_struct_methods() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "go.mod", "module example.com/m\n");
        let a = write(
            root,
            "pkg/conn.go",
            "package pkg\n\ntype Conn struct{}\n\nfunc (c *Conn) Send() int { return 1 }\n",
        );
        let b = write(
            root,
            "pkg/extra.go",
            "package pkg\n\nfunc (c *Conn) Retry() int { return c.Send() }\n",
        );
        let on = run(root, &[a, b], true);
        let c = calls(&on);
        assert!(
            has(&c, "pkg_conn_retry", "pkg_conn_send", "self_method"),
            "{c:?}"
        );
    }
}
