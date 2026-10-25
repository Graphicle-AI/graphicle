// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/resolution.py
//! Cross-file Python, Java, PHP and Go type binding and declaration merging.

#[cfg(test)]
pub struct PythonResolutionContext<'n, 'a> {
    pub path: &'a Path,
    pub source: &'a [u8],
    /// Outlives the walk so the current owner can be borrowed from it.
    pub names: &'n BTreeMap<String, String>,
    pub entities: &'a BTreeMap<String, BTreeMap<String, String>>,
    /// `entities` keys by the tails that follow a separator in them.
    pub tails: &'a TailIndex<'a>,
    pub bare: &'a BTreeMap<String, String>,
    pub imports: &'a mut BTreeMap<String, String>,
    pub uses: &'a mut BTreeMap<String, BTreeMap<String, usize>>,
    /// Absolute module ids naming more than one scanned file (#3729).
    pub ambiguous: &'a super::resolution_facts::AmbiguousModules,
    /// The stems of the file's own sources: a module never imports itself.
    pub own_stems: &'a BTreeSet<String>,
}
use super::{
    base::{file_stem, make_id, node_bytes, read_text},
    engine_parser::Parsed,
    engine_types::KindName,
    engine_types::{children, first_child},
    parse_cache,
    resolution::{is_type_like_definition, resolved, s},
    resolution_facts::{parse_python_tree, walk},
};
use crate::resolver_registry::rs;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
};
use tree_sitter::{Language, Node};
fn parse(path: &Path, tag: &'static str, language: Language) -> Option<parse_cache::Tree> {
    parse_cache::parse(path, tag, language)
}
fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}
pub fn prune_repointed(nodes: &mut Vec<Value>, edges: &[Value], old: &BTreeSet<String>) {
    let refs = edges
        .iter()
        .flat_map(|e| [s(e, "source"), s(e, "target")])
        .collect::<BTreeSet<_>>();
    nodes.retain(|n| !old.contains(s(n, "id")) || refs.contains(s(n, "id")));
}
/// `_merge_go_package_types` (#3399): a Go package's type declared in one
/// file and the copies minted by the other files that declare methods on it
/// share one id (the extractor keys a type on its package directory), so they
/// fold into the declaring file's node before disambiguation would split
/// them. Only `.go` nodes of one directory fold (the id holds only the
/// directory's name), and never with a `_test.go` member (its package clause
/// may be the separate `_test` package). Edges already point at the shared id.
pub fn merge_go_package_types(nodes: &mut Vec<Value>, edges: &[Value]) {
    let mut by_id: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for (i, n) in nodes.iter().enumerate() {
        let id = s(n, "id");
        let label = s(n, "label");
        if id.is_empty() || s(n, "file_type") != "code" {
            continue;
        }
        // A Go type name holds neither character; every other label the
        // extractor mints does (`Run()`, `.Close()`, the file's `a.go`).
        if label.is_empty() || label.contains('.') || label.contains('(') {
            continue;
        }
        let go = Path::new(s(n, "source_file"))
            .extension()
            .is_some_and(|e| e.to_string_lossy().to_lowercase() == "go");
        if go {
            by_id.entry(id).or_default().push(i);
        }
    }
    by_id.retain(|_, group| group.len() >= 2);
    if by_id.is_empty() {
        return;
    }
    let mut declaring: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
    for e in edges {
        if s(e, "relation") == "contains" && by_id.contains_key(s(e, "target")) {
            declaring
                .entry(s(e, "target"))
                .or_default()
                .insert(s(e, "source_file"));
        }
    }
    let mut dropped = FxHashSet::default();
    for (id, group) in &by_id {
        let files = group
            .iter()
            .map(|&i| Path::new(s(&nodes[i], "source_file")))
            .collect::<Vec<_>>();
        if files.iter().any(|f| f.parent() != files[0].parent()) {
            continue;
        }
        if files.iter().any(|f| {
            f.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_test.go"))
        }) {
            continue;
        }
        // A type whose declaring file is outside the corpus leaves the group
        // method-only; it still folds, onto the lowest path.
        let declared = declaring.get(id);
        let mut candidates = group
            .iter()
            .copied()
            .filter(|&i| declared.is_some_and(|d| d.contains(s(&nodes[i], "source_file"))))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            candidates = group.clone();
        }
        let keeper = candidates
            .into_iter()
            .min_by(|&a, &b| {
                (s(&nodes[a], "source_file"), s(&nodes[a], "source_location"))
                    .cmp(&(s(&nodes[b], "source_file"), s(&nodes[b], "source_location")))
            })
            .unwrap();
        dropped.extend(group.iter().copied().filter(|&i| i != keeper));
    }
    drop(declaring);
    drop(by_id);
    if !dropped.is_empty() {
        let mut index = 0;
        nodes.retain(|_| {
            let keep = !dropped.contains(&index);
            index += 1;
            keep
        });
    }
}
pub fn merge_decl_def_classes(nodes: &mut Vec<Value>, edges: &mut Vec<Value>) {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if s(n, "file_type") == "code" && !s(n, "id").is_empty() && !s(n, "source_file").is_empty()
        {
            groups.entry(s(n, "id").into()).or_default().push(i);
        }
    }
    let mut drop: BTreeSet<usize> = BTreeSet::new();
    for indices in groups.values().filter(|v| v.len() > 1) {
        let mut keys = BTreeSet::new();
        let mut headers = vec![];
        let mut good = true;
        for i in indices {
            let p = Path::new(s(&nodes[*i], "source_file"));
            let ext = p
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if !["h", "hpp", "hh", "hxx", "m", "mm", "cpp", "cc", "cxx", "c"]
                .contains(&ext.as_str())
            {
                good = false;
                break;
            }
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            keys.insert((
                p.parent().unwrap_or(Path::new(".")).to_path_buf(),
                stem.split('+').next().unwrap_or("").to_string(),
            ));
            if ["h", "hpp", "hh", "hxx"].contains(&ext.as_str()) {
                headers.push(*i);
            }
        }
        if !good || keys.len() != 1 || headers.is_empty() {
            continue;
        }
        let bases = headers
            .iter()
            .filter(|i| {
                !Path::new(s(&nodes[**i], "source_file"))
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains('+')
            })
            .copied()
            .collect::<Vec<_>>();
        if bases.len() > 1 {
            continue;
        }
        headers.sort_by_key(|i| {
            Path::new(s(&nodes[*i], "source_file"))
                .file_stem()
                .unwrap_or_default()
                .to_os_string()
        });
        let keep = bases.first().copied().unwrap_or(headers[0]);
        let mut impls = indices
            .iter()
            .filter(|i| **i != keep && !headers.contains(i))
            .copied()
            .collect::<Vec<_>>();
        impls.sort_by_key(|i| {
            (
                s(&nodes[*i], "source_file").to_string(),
                s(&nodes[*i], "source_location").to_string(),
            )
        });
        if let Some(i) = impls.first() {
            let file = nodes[*i]["source_file"].clone();
            let loc = nodes[*i]["source_location"].clone();
            nodes[keep]["definition_file"] = file;
            if loc.as_str().is_some_and(|s| !s.is_empty()) {
                nodes[keep]["definition_location"] = loc;
            }
        }
        drop.extend(indices.iter().filter(|i| **i != keep).copied());
    }
    if drop.is_empty() {
        return;
    }
    let mut i = 0;
    nodes.retain(|_| {
        let keep = !drop.contains(&i);
        i += 1;
        keep
    });
    let mut seen = BTreeSet::new();
    edges.retain(|e| {
        s(e, "source") != s(e, "target")
            && seen.insert((
                s(e, "source").to_string(),
                s(e, "target").to_string(),
                s(e, "relation").to_string(),
                e.get("context").cloned().unwrap_or(Value::Null).to_string(),
            ))
    });
}
/// What [`cross_file_python_imports`] reads from one Python file's tree:
/// its `from ... import ...` statements in walk order and, per chain of
/// enclosing named class/function definitions (outermost first), the first
/// line of each identifier. Recorded by the symbol-resolution pass from the
/// tree it already parsed ([`record_python_visit`]), so the file is not
/// parsed a second time; [`replay_python_visit`] then answers exactly what a
/// walk of the tree would (see `visit`).
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct PythonVisitFacts {
    imports: Vec<ImportFromFacts>,
    scopes: Vec<(Vec<String>, BTreeMap<String, usize>)>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ImportFromFacts {
    /// The statement's `relative_import` (true) and `dotted_name` (false)
    /// children, in order, with their text.
    parts: Vec<(bool, String)>,
    names: Vec<(String, String)>,
}
/// The kind under which [`PythonVisitFacts`] are kept in the run cache.
pub const PYTHON_VISIT_FACTS: &str = "python_visit";
pub fn record_python_visit(root: Node<'_>, source: &[u8]) -> PythonVisitFacts {
    record_python_syntax(None, root, source).0
}
/// A Python file's `from ... import ...` statements as the symbol-resolution
/// pass reads them: `(level, module, line, (imported, local) names)`.
pub type PythonImportFroms = Vec<(usize, String, i64, Vec<(String, String)>)>;
/// Calls in top-level function bodies: `(owner id, callee, line)`.
pub type PythonBodyCalls = Vec<(String, String, i64)>;
/// One walk of a Python tree for both the symbol-resolution pass (its import
/// statements and, given the file's path, the calls in its top-level function
/// bodies) and the cross-file import pass ([`PythonVisitFacts`]), in the
/// order separate walks of the tree would list them.
pub fn record_python_syntax(
    path: Option<&Path>,
    root: Node<'_>,
    source: &[u8],
) -> (PythonVisitFacts, PythonImportFroms, PythonBodyCalls) {
    let mut walk = SyntaxWalk {
        source,
        facts: PythonVisitFacts::default(),
        chain: Vec::new(),
        scope: Vec::new(),
        index: HashMap::new(),
        import_froms: Vec::new(),
        calls: Vec::new(),
        bodies: HashMap::new(),
        body: None,
    };
    if let Some(path) = path {
        // `top_bodies(path, root, source, true)`: each top-level function's
        // body, owned by `<file stem>_<name>`.
        let stem = file_stem(path);
        let mut cursor = root.walk();
        for n in root.children(&mut cursor) {
            if n.kind_name() == "function_definition"
                && let (Some(name), Some(body)) =
                    (n.child_by_field_name("name"), n.child_by_field_name("body"))
            {
                walk.bodies.insert(
                    body.id(),
                    make_id([stem.as_str(), read_text(name, source).as_str()]),
                );
            }
        }
    }
    walk.visit(root);
    (walk.facts, walk.import_froms, walk.calls)
}
struct SyntaxWalk<'s> {
    source: &'s [u8],
    facts: PythonVisitFacts,
    chain: Vec<String>,
    scope: Vec<usize>,
    index: HashMap<Vec<String>, usize>,
    import_froms: PythonImportFroms,
    calls: PythonBodyCalls,
    /// Top-level function bodies by node id, with their owner ids.
    bodies: HashMap<usize, String>,
    /// The owner of the top-level body being walked.
    body: Option<String>,
}
impl SyntaxWalk<'_> {
    fn visit(&mut self, n: Node<'_>) {
        let entered = self
            .body
            .is_none()
            .then(|| self.bodies.get(&n.id()).cloned())
            .flatten();
        if entered.is_some() {
            self.body = entered;
        }
        record_visit(n, self);
        if self.body.is_some() && self.bodies.contains_key(&n.id()) {
            self.body = None;
        }
    }
}
fn record_visit(n: Node<'_>, walk: &mut SyntaxWalk<'_>) {
    let source = walk.source;
    if let Some(owner) = &walk.body
        && let Some(name) = super::resolution_facts::call_identifier(n, source, true)
    {
        walk.calls
            .push((owner.clone(), name, (n.start_position().row + 1) as i64));
    }
    let kind = n.kind_name();
    if kind == "import_from_statement" {
        let names = super::resolution_facts::python_imported_names(n, source);
        if let Some((level, module)) = super::resolution_facts::python_import_from_module(n, source)
        {
            walk.import_froms.push((
                level,
                module,
                (n.start_position().row + 1) as i64,
                names.clone(),
            ));
        }
        let parts = children(n)
            .into_iter()
            .filter_map(|c| match c.kind_name() {
                "relative_import" => Some((true, read_text(c, source))),
                "dotted_name" => Some((false, read_text(c, source))),
                _ => None,
            })
            .collect();
        walk.facts.imports.push(ImportFromFacts { parts, names });
        return;
    }
    let definition = matches!(kind, "class_definition" | "function_definition")
        .then(|| n.child_by_field_name("name"))
        .flatten();
    if let Some(name) = definition {
        walk.chain.push(read_text(name, source));
        let at = match walk.index.get(walk.chain.as_slice()) {
            Some(at) => *at,
            None => {
                walk.facts
                    .scopes
                    .push((walk.chain.clone(), BTreeMap::new()));
                walk.index
                    .insert(walk.chain.clone(), walk.facts.scopes.len() - 1);
                walk.facts.scopes.len() - 1
            }
        };
        walk.scope.push(at);
    }
    if kind == "identifier"
        && let Some(&at) = walk.scope.last()
    {
        let ident = node_bytes(n, source);
        let rows = &mut walk.facts.scopes[at].1;
        let known = std::str::from_utf8(ident).is_ok_and(|ident| rows.contains_key(ident));
        if !known {
            rows.entry(String::from_utf8_lossy(ident).into_owned())
                .or_insert(n.start_position().row + 1);
        }
    }
    let mut cursor = n.walk();
    for c in n.children(&mut cursor) {
        walk.visit(c);
    }
    if definition.is_some() {
        walk.chain.pop();
        walk.scope.pop();
    }
}
/// The `imports` (local name -> id) and `uses` (identifier -> owner id ->
/// first line) that `visit` collects from the recorded file. A statement whose
/// module resolves to one of `own_stems` binds nothing: a module cannot import
/// its own names (#4225, the bare-stem fallback matching the importing file).
#[allow(clippy::too_many_arguments)]
fn replay_python_visit(
    facts: &PythonVisitFacts,
    path: &Path,
    names: &BTreeMap<String, String>,
    entities: &BTreeMap<String, BTreeMap<String, String>>,
    tails: &TailIndex<'_>,
    bare: &BTreeMap<String, String>,
    ambiguous: &super::resolution_facts::AmbiguousModules,
    own_stems: &BTreeSet<String>,
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, BTreeMap<String, usize>>,
) {
    let mut imports = BTreeMap::new();
    'statements: for statement in &facts.imports {
        let mut target = None;
        let mut absolute = None;
        for (relative, raw) in &statement.parts {
            if *relative {
                let dots = raw.chars().take_while(|c| *c == '.').count().max(1);
                let module = raw.trim_start_matches('.');
                let mut dir = path.parent().unwrap_or(Path::new("."));
                for _ in 1..dots {
                    dir = dir.parent().unwrap_or(dir);
                }
                target = Some(file_stem(&if module.is_empty() {
                    dir.join("__init__.py")
                } else {
                    dir.join(module.replace('.', "/")).with_extension("py")
                }));
                break;
            }
            if target.is_none() {
                // An ambiguous absolute module binds none of its names.
                if ambiguous.contains(&make_id([raw.as_str()])) {
                    continue 'statements;
                }
                let module = raw.replace('.', "/");
                if entities.contains_key(&module) {
                    target = Some(module);
                } else {
                    target = match tails.get(module.as_str()) {
                        Some((1, key)) => Some((*key).clone()),
                        _ => bare.get(raw.rsplit('.').next().unwrap_or("")).cloned(),
                    };
                }
                absolute = Some(raw.clone());
            }
        }
        if let Some(map) = target
            .as_ref()
            .filter(|t| !own_stems.contains(*t))
            .and_then(|t| entities.get(t))
        {
            for (imported, local) in &statement.names {
                if let Some(module) = &absolute
                    && ambiguous.contains(&make_id([format!("{module}.{imported}").as_str()]))
                {
                    continue;
                }
                if let Some(id) = map.get(imported) {
                    imports.insert(local.clone(), id.clone());
                }
            }
        }
    }
    // An identifier belongs to the outermost enclosing definition named in
    // `names`; its first line under that owner is the smallest line of any
    // chain resolving to it (a walk meets identifiers in line order).
    let mut uses: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for (chain, rows) in &facts.scopes {
        let Some(owner) = chain.iter().find_map(|name| names.get(name)) else {
            continue;
        };
        for (ident, row) in rows {
            let first = uses
                .entry(ident.clone())
                .or_default()
                .entry(owner.clone())
                .or_insert(*row);
            *first = (*first).min(*row);
        }
    }
    (imports, uses)
}
/// For every tail that follows a `/` or `\` in an `entities` key: how many
/// keys end with `/<tail>` or `\<tail>`, and the first of them in key order.
/// A key contributes each tail once (each separator starts a different tail),
/// so a lookup answers "which keys end with a separator plus this module" as a
/// scan of every key would.
pub type TailIndex<'a> = HashMap<&'a str, (usize, &'a String)>;
fn tail_index(entities: &BTreeMap<String, BTreeMap<String, String>>) -> TailIndex<'_> {
    let mut index = TailIndex::new();
    for key in entities.keys() {
        for (i, b) in key.bytes().enumerate() {
            if b == b'/' || b == b'\\' {
                index.entry(&key[i + 1..]).or_insert((0, key)).0 += 1;
            }
        }
    }
    index
}
#[cfg(test)]
fn visit<'n>(n: Node<'_>, current: Option<&'n str>, context: PythonResolutionContext<'n, '_>) {
    let PythonResolutionContext {
        path,
        source,
        names,
        entities,
        tails,
        bare,
        imports,
        uses,
        ambiguous,
        own_stems,
    } = context;

    if n.kind_name() == "import_from_statement" {
        let mut target = None;
        let mut absolute = None;
        for c in children(n) {
            if c.kind_name() == "relative_import" {
                let raw = read_text(c, source);
                let dots = raw.chars().take_while(|c| *c == '.').count().max(1);
                let module = raw.trim_start_matches('.');
                let mut dir = path.parent().unwrap_or(Path::new("."));
                for _ in 1..dots {
                    dir = dir.parent().unwrap_or(dir);
                }
                target = Some(file_stem(&if module.is_empty() {
                    dir.join("__init__.py")
                } else {
                    dir.join(module.replace('.', "/")).with_extension("py")
                }));
                break;
            }
            if c.kind_name() == "dotted_name" && target.is_none() {
                let raw = read_text(c, source);
                // An ambiguous absolute module binds none of its names.
                if ambiguous.contains(&make_id([raw.as_str()])) {
                    return;
                }
                let module = raw.replace('.', "/");
                if entities.contains_key(&module) {
                    target = Some(module);
                } else {
                    // The one key ending with `/<module>` or `\<module>`, if
                    // exactly one does.
                    target = match tails.get(module.as_str()) {
                        Some((1, key)) => Some((*key).clone()),
                        _ => bare.get(raw.rsplit('.').next().unwrap_or("")).cloned(),
                    };
                }
                absolute = Some(raw);
            }
        }
        if let Some(map) = target
            .as_ref()
            .filter(|t| !own_stems.contains(*t))
            .and_then(|t| entities.get(t))
        {
            for (imported, local) in super::resolution_facts::python_imported_names(n, source) {
                if let Some(module) = &absolute
                    && ambiguous.contains(&make_id([format!("{module}.{imported}").as_str()]))
                {
                    continue;
                }
                if let Some(id) = map.get(&imported) {
                    imports.insert(local, id.clone());
                }
            }
        }
        return;
    }
    let mut current = current;
    if current.is_none()
        && matches!(n.kind_name(), "class_definition" | "function_definition")
        && let Some(name) = n.child_by_field_name("name")
    {
        current = names.get(&read_text(name, source)).map(String::as_str);
    }
    if n.kind_name() == "identifier"
        && let Some(id) = current
    {
        // Identifiers dominate the tree; only allocate when this
        // (identifier, owner) pair is new.
        let ident = node_bytes(n, source);
        let known = std::str::from_utf8(ident)
            .ok()
            .and_then(|ident| uses.get_mut(ident))
            .is_some_and(|owners| owners.contains_key(id));
        if !known {
            uses.entry(String::from_utf8_lossy(ident).into_owned())
                .or_default()
                .entry(id.to_string())
                .or_insert(n.start_position().row + 1);
        }
    }
    let mut cursor = n.walk();
    for c in n.children(&mut cursor) {
        visit(
            c,
            current,
            PythonResolutionContext {
                path,
                source,
                names,
                entities,
                tails,
                bare,
                imports: &mut *imports,
                uses: &mut *uses,
                ambiguous,
                own_stems,
            },
        );
    }
}
pub fn cross_file_python_imports(
    per_file: &[Value],
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut [Value],
    ambiguous: &super::resolution_facts::AmbiguousModules,
) -> Vec<Value> {
    let mut entities: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut bare = BTreeMap::new();
    for result in per_file {
        for n in result["nodes"].as_array().into_iter().flatten() {
            let source = s(n, "source_file");
            let label = s(n, "label");
            if source.is_empty() {
                continue;
            }
            let p = Path::new(source);
            let stem = file_stem(p);
            if !label.is_empty()
                && !label.ends_with(')')
                && !label.ends_with(".py")
                && !label.starts_with('_')
                && s(n, "file_type") != "rationale"
            {
                entities
                    .entry(stem.clone())
                    .or_default()
                    .insert(label.into(), s(n, "id").into());
                bare.entry(
                    p.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
                .or_insert(stem);
            }
        }
    }
    // Borrow rather than clone every node; only stub lookups by id are needed.
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<HashMap<&str, &Value>>();
    // Index repointable edges by source_file once instead of scanning every
    // edge for every file (the scan was O(files x edges)).
    let mut edges_by_source: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        if ["references", "inherits", "implements", "extends"].contains(&s(e, "relation")) {
            edges_by_source
                .entry(s(e, "source_file").to_string())
                .or_default()
                .push(i);
        }
    }
    let tails = tail_index(&entities);
    let mut repointed = BTreeSet::new();
    let mut out = vec![];
    // The per-file tree walks only read shared tables, so they run in
    // parallel; the edge repointing below stays sequential in path order.
    let inputs = per_file.iter().zip(paths).collect::<Vec<_>>();
    let walked = parse_cache::par_map(&inputs, |(result, path)| {
        let path_str = path.to_string_lossy();
        let mut names = BTreeMap::new();
        let mut sources = BTreeSet::from([path_str.to_string()]);
        for n in result["nodes"].as_array().into_iter().flatten() {
            if !s(n, "source_file").is_empty() {
                sources.insert(s(n, "source_file").to_string());
            }
            if s(n, "source_file") == path_str
                && s(n, "file_type") != "rationale"
                && !s(n, "label").ends_with(".py")
            {
                let label = s(n, "label").strip_suffix("()").unwrap_or(s(n, "label"));
                if !label.is_empty() {
                    names.entry(label.into()).or_insert(s(n, "id").to_string());
                }
            }
        }
        // The symbol-resolution pass recorded what this walk reads from the
        // file's tree; without that record the file is parsed here.
        let kept = parse_cache::take_facts(PYTHON_VISIT_FACTS, path)
            .and_then(|facts| facts.downcast::<PythonVisitFacts>().ok());
        if names.is_empty() {
            return None;
        }
        let facts = match kept {
            Some(facts) => *facts,
            None => {
                let parsed = parse_python_tree(path)?;
                record_python_visit(parsed.tree.root_node(), &parsed.source)
            }
        };
        let own_stems = sources
            .iter()
            .map(|source| file_stem(Path::new(source)))
            .collect::<BTreeSet<_>>();
        let (imports, uses) = replay_python_visit(
            &facts, path, &names, &entities, &tails, &bare, ambiguous, &own_stems,
        );
        Some((*path, sources, imports, uses))
    });
    for (path, sources, imports, uses) in walked.into_iter().flatten() {
        for (name, target) in &imports {
            for (src, line) in uses.get(name).into_iter().flatten() {
                if src != target {
                    out.push(json!({"source":src,"target":target,"relation":"uses","confidence":"INFERRED","confidence_score":0.95,"source_file":path,"source_location":format!("L{line}"),"weight":0.8}));
                }
            }
        }
        let mut candidates = sources
            .iter()
            .filter_map(|src| edges_by_source.get(src))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        for i in candidates {
            let e = &mut edges[i];
            let old = s(e, "target").to_string();
            if let Some(stub) = by_id
                .get(old.as_str())
                .filter(|n| s(n, "source_file").is_empty())
                && let Some(target) = imports.get(s(stub, "label")).filter(|t| **t != old)
            {
                e["target"] = json!(target);
                repointed.insert(old);
            }
        }
    }
    prune_repointed(nodes, edges, &repointed);
    out
}
/// What the Java passes read from one file's tree, in walk order: the text of
/// its package declarations and of its import declarations with their lines.
/// A file's extraction keeps this when it parsed the file's own bytes, and the
/// AST cache stores it beside the file's record, so the passes do not parse
/// the file again.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct JavaSyntax {
    packages: Vec<String>,
    imports: Vec<(String, i64)>,
}
/// The kind under which [`JavaSyntax`] is kept in the run cache.
pub const JAVA_SYNTAX: &str = "java_syntax";
pub fn java_syntax(parsed: &Parsed) -> JavaSyntax {
    let mut out = JavaSyntax::default();
    for n in walk(parsed.tree.root_node()) {
        match n.kind_name() {
            "package_declaration" => out.packages.push(read_text(n, &parsed.source)),
            "import_declaration" => out.imports.push((
                read_text(n, &parsed.source),
                (n.start_position().row + 1) as i64,
            )),
            _ => {}
        }
    }
    out
}
impl JavaSyntax {
    pub fn encode(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&(1u32, self)).ok()
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (format, syntax): (u32, Self) = serde_json::from_slice(bytes).ok()?;
        (format == 1).then_some(syntax)
    }
}
/// `import a.b.C;` as the passes compare it: `a.b.C`.
fn java_import_path(raw: &str) -> &str {
    raw.trim()
        .trim_start_matches("import")
        .trim()
        .trim_end_matches(';')
        .trim()
        .trim_start_matches("static ")
        .trim()
}
fn java_info(syntax: &JavaSyntax) -> (String, BTreeMap<String, String>) {
    let mut package = String::new();
    if let Some(raw) = syntax.packages.last() {
        package = raw
            .trim()
            .trim_start_matches("package")
            .trim()
            .trim_end_matches(';')
            .trim()
            .into();
    }
    let mut imports = BTreeMap::new();
    for (raw, _) in &syntax.imports {
        let raw = java_import_path(raw);
        if raw.ends_with(".*") || !raw.contains('.') {
            continue;
        }
        let name = raw.rsplit('.').next().unwrap_or("");
        if upper(name) {
            imports.insert(name.into(), raw.into());
        }
    }
    (package, imports)
}
/// Each Java file's [`JavaSyntax`]: the facts its extraction (or the AST
/// cache) kept for this run, else parsed here, files in parallel. `take`
/// releases the kept facts; the first of the two passes leaves them for the
/// second.
fn java_syntaxes(paths: &[PathBuf], take: bool) -> Vec<Option<JavaSyntax>> {
    parse_cache::par_map(paths, |p| {
        let kept = if take {
            parse_cache::take_facts(JAVA_SYNTAX, p)
                .and_then(|f| f.downcast::<JavaSyntax>().ok())
                .map(|f| *f)
        } else {
            parse_cache::kept_clone::<JavaSyntax>(JAVA_SYNTAX, p)
        };
        kept.or_else(|| {
            parse(p, "java", tree_sitter_java::LANGUAGE.into()).map(|parsed| java_syntax(&parsed))
        })
    })
}
/// Cross-file Java `imports` edges, from each file's import declarations to
/// the declared types they name. The per-file results are read in the form
/// assembly holds them (no JSON copy), and the label table borrows them.
pub fn cross_file_java_imports<P: crate::extract::per_file::PerFile>(
    per_file: &[P],
    paths: &[PathBuf],
) -> Vec<Value> {
    let mut trees = BTreeMap::new();
    let mut packages = BTreeMap::new();
    for ((p, r), syntax) in paths.iter().zip(per_file).zip(java_syntaxes(paths, true)) {
        if let Some(parsed) = syntax {
            let (pkg, _) = java_info(&parsed);
            packages.insert(p.to_string_lossy().into_owned(), pkg.clone());
            for n in r.nodes().into_iter().flatten() {
                let source = rs(n, "source_file");
                if !source.is_empty() && !packages.contains_key(source) {
                    packages.insert(source.to_string(), pkg.clone());
                }
            }
            trees.insert(p.clone(), parsed);
        }
    }
    let mut labels: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for r in per_file {
        for n in r.nodes().into_iter().flatten() {
            let label = rs(n, "label");
            if upper(label)
                && !label.ends_with(')')
                && !label.ends_with(".java")
                && !rs(n, "source_file").is_empty()
                && !rs(n, "id").is_empty()
            {
                labels.entry(label).or_default().push((
                    rs(n, "id"),
                    packages
                        .get(rs(n, "source_file"))
                        .map_or("", String::as_str),
                ));
            }
        }
    }
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    for p in paths {
        let Some(parsed) = trees.get(p) else { continue };
        let file = make_id([p.to_string_lossy().as_ref()]);
        for (raw, line) in &parsed.imports {
            let raw = java_import_path(raw);
            if raw.ends_with(".*") {
                continue;
            }
            let mut parts = raw.split('.').collect::<Vec<_>>();
            let mut last = parts.pop().unwrap_or("");
            if last.chars().next().is_some_and(char::is_lowercase) {
                last = parts.pop().unwrap_or(last);
            }
            let pkg = parts.join(".");
            for &(target, target_pkg) in labels.get(last).into_iter().flatten() {
                let matches = pkg == target_pkg
                    || pkg
                        .strip_prefix(&if target_pkg.is_empty() {
                            String::new()
                        } else {
                            format!("{target_pkg}.")
                        })
                        .is_some_and(|s| !s.is_empty() && s.split('.').all(upper));
                if target != file && matches && seen.insert((file.clone(), target.to_string())) {
                    out.push(json!({"source":file,"target":target,"relation":"imports","confidence":"EXTRACTED","confidence_score":1.0,"source_file":p,"source_location":format!("L{line}"),"weight":1.0}));
                }
            }
        }
    }
    out
}
/// Bind the Java `implements`, `inherits`, `extends`, `imports` and
/// `references` edges that end at a stub to the declared type the stub's name
/// means. The per-file results are read in the form assembly holds them (no
/// JSON copy), and the node tables borrow the node list instead of copying
/// it: nothing changes `nodes` until the pass adds its new stubs.
pub fn resolve_java_type_references<P: crate::extract::per_file::PerFile>(
    per_file: &[P],
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    let mut packages = BTreeMap::new();
    let mut imports = BTreeMap::new();
    for (r, syntax) in per_file.iter().zip(java_syntaxes(paths, false)) {
        let Some(parsed) = syntax else {
            continue;
        };
        let (pkg, imps) = java_info(&parsed);
        // Every node of the file gives its source the same facts, so a source
        // repeated in a row is stored once.
        let mut last = None;
        for n in r.nodes().into_iter().flatten() {
            let source = rs(n, "source_file");
            if !source.is_empty() && last != Some(source) {
                packages.insert(source.to_string(), pkg.clone());
                imports.insert(source.to_string(), imps.clone());
                last = Some(source);
            }
        }
    }
    let mut added = vec![];
    let mut old = BTreeSet::new();
    {
        // Each id's node (the last of equal ids, as collecting a map keeps it).
        let mut by_id = FxHashMap::default();
        for n in nodes.iter() {
            by_id.insert(s(n, "id"), n);
        }
        let mut parents = FxHashMap::default();
        for e in edges.iter().filter(|e| s(e, "relation") == "contains") {
            if let (Some(child), Some(parent)) = (
                by_id.get(s(e, "target")).copied(),
                by_id.get(s(e, "source")).copied(),
            ) && !s(child, "source_file").is_empty()
                && s(parent, "source_file") == s(child, "source_file")
                && upper(s(parent, "label"))
                && !s(parent, "label").ends_with(".java")
            {
                parents.insert(s(child, "id"), s(parent, "id"));
            }
        }
        let mut fqns = FxHashMap::default();
        for n in nodes.iter() {
            let (label, src, id) = (s(n, "label"), s(n, "source_file"), s(n, "id"));
            if !upper(label) || label.ends_with(')') || label.ends_with(".java") || id.is_empty() {
                continue;
            }
            let Some(pkg) = packages.get(src) else {
                continue;
            };
            let prefix = if pkg.is_empty() {
                String::new()
            } else {
                format!("{pkg}.")
            };
            fqns.entry(format!("{prefix}{label}")).or_insert(id);
            let mut names = vec![label];
            let mut seen = BTreeSet::from([id]);
            let mut parent = parents.get(id).copied();
            while let Some(p) = parent {
                if !seen.insert(p) {
                    break;
                }
                if let Some(n) = by_id.get(p) {
                    names.push(s(n, "label"));
                }
                parent = parents.get(p).copied();
            }
            if names.len() > 1 {
                names.reverse();
                fqns.entry(format!("{prefix}{}", names.join(".")))
                    .or_insert(id);
            }
        }
        drop(parents);
        let mut stubs = FxHashMap::default();
        for n in nodes.iter().filter(|n| {
            s(n, "source_file").is_empty() && (upper(s(n, "label")) || s(n, "label").contains('.'))
        }) {
            stubs.insert(s(n, "id"), s(n, "label"));
        }
        // The ids of the stubs this pass adds (the nodes' own are `by_id`'s).
        let mut new_ids = BTreeSet::new();
        for e in edges.iter_mut() {
            if !["implements", "inherits", "extends", "imports", "references"]
                .contains(&s(e, "relation"))
            {
                continue;
            }
            let Some(&label) = stubs.get(s(e, "target")) else {
                continue;
            };
            let target = s(e, "target").to_string();
            let src = s(e, "source_file");
            let mut explicit = false;
            let fqn = if label.contains('.') {
                label.to_string()
            } else if let Some(fqn) = imports.get(src).and_then(|m| m.get(label)) {
                explicit = true;
                fqn.clone()
            } else {
                let pkg = packages.get(src).map(String::as_str).unwrap_or("");
                if pkg.is_empty() {
                    label.to_string()
                } else {
                    format!("{pkg}.{label}")
                }
            };
            let mut bound = fqns.get(&fqn).map(|id: &&str| id.to_string());
            if explicit && bound.is_none() {
                let mut head = fqn.split('.').collect::<Vec<_>>();
                head.pop();
                while head.last().is_some_and(|s| upper(s)) && bound.is_none() {
                    head.pop();
                    let mut p = head.clone();
                    p.push(label);
                    bound = fqns.get(&p.join(".")).map(|id: &&str| id.to_string());
                }
            }
            if explicit && bound.is_none() {
                let id = make_id([fqn.as_str()]);
                if !by_id.contains_key(id.as_str()) && new_ids.insert(id.clone()) {
                    added.push(json!({"id":id,"label":fqn,"file_type":"code","source_file":"","source_location":""}));
                }
                bound = Some(id);
            }
            if let Some(id) = bound.filter(|id| *id != target) {
                e["target"] = json!(id);
                old.insert(target);
            }
        }
    }
    drop(imports);
    nodes.extend(added);
    // Each Java attribute reference is kept once: the first of equal ones.
    let keep = {
        let mut seen = FxHashSet::default();
        edges
            .iter()
            .map(|e| {
                !(s(e, "relation") == "references"
                    && s(e, "context") == "attribute"
                    && packages.contains_key(s(e, "source_file")))
                    || seen.insert(
                        [
                            "source",
                            "target",
                            "relation",
                            "context",
                            "source_file",
                            "source_location",
                        ]
                        .map(|k| s(e, k)),
                    )
            })
            .collect::<Vec<_>>()
    };
    drop(packages);
    let mut keep = keep.into_iter();
    edges.retain(|_| keep.next().unwrap_or(true));
    drop(keep);
    prune_repointed(nodes, edges, &old);
}
/// The import path of the Go package that holds `source` (relative paths
/// are taken from `root`): the nearest `go.mod` above the resolved file
/// names the module. A `go.mod` that cannot be read, or that has no `module`
/// line, gives `None` and the search goes no higher.
///
/// A pass that asks for many files keeps one [`GoModules`] instead.
pub fn go_import_path_for_file(source: &Path, root: &Path) -> Option<String> {
    GoModules::new(root).import_path(source).map(str::to_owned)
}
/// One directory's `go.mod`, as [`go_import_path_for_file`] reads it.
enum GoMod {
    /// No `go.mod` file here: the search goes on to the parent directory.
    Absent,
    /// A `go.mod` that cannot be read or names no module: no import path.
    Unusable,
    /// The module path it declares.
    Module(String),
}
impl GoMod {
    fn probe(dir: &Path) -> Self {
        static MODULE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?m)^\s*module\s+([^\s]+)").expect("constant Go module regex")
        });
        let gm = dir.join("go.mod");
        if !gm.is_file() {
            return GoMod::Absent;
        }
        let Ok(raw) = std::fs::read_to_string(gm) else {
            return GoMod::Unusable;
        };
        MODULE
            .captures(&raw)
            .map_or(GoMod::Unusable, |cap| GoMod::Module(cap[1].into()))
    }
}
/// [`go_import_path_for_file`] for one pass over a corpus: each directory's
/// `go.mod` is probed and read at most once and each source path resolved
/// once, where the plain function does both on every call. Bind calls ask
/// once per candidate of every Go call, so without it the same `go.mod`
/// files are read millions of times on a large module tree.
///
/// Build one per pass and drop it with the pass: it never sees a change made
/// on disk after it read a file, so an update or a watch rebuild must start
/// from a new one.
pub struct GoModules<'r> {
    root: &'r Path,
    /// The `go.mod` of every directory probed so far.
    mods: FxHashMap<PathBuf, GoMod>,
    /// The import path of every resolved source directory seen so far.
    dirs: FxHashMap<PathBuf, Option<String>>,
    /// The import path of every source path asked for, by its exact bytes:
    /// bind calls ask once per candidate (12.8 million times on kubernetes),
    /// and hashing the bytes is several times cheaper than hashing a `Path`
    /// component by component. Spellings that differ only in separators get
    /// separate entries with the same answer.
    files: FxHashMap<OsString, Option<String>>,
}
impl<'r> GoModules<'r> {
    pub fn new(root: &'r Path) -> Self {
        Self {
            root,
            mods: FxHashMap::default(),
            dirs: FxHashMap::default(),
            files: FxHashMap::default(),
        }
    }
    /// The import path of the package holding `source`, as
    /// [`go_import_path_for_file`] gives it for this pass's root.
    pub fn import_path(&mut self, source: &Path) -> Option<&str> {
        let key = source.as_os_str();
        if !self.files.contains_key(key) {
            let path = self.file(source);
            self.files.insert(key.to_owned(), path);
        }
        self.files[key].as_deref()
    }
    fn file(&mut self, source: &Path) -> Option<String> {
        let p = if source.is_absolute() {
            source.to_path_buf()
        } else {
            self.root.join(source)
        };
        let p = resolved(&p);
        let dir = p.parent()?;
        if let Some(path) = self.dirs.get(dir) {
            return path.clone();
        }
        let path = self.directory(dir);
        self.dirs.insert(dir.to_path_buf(), path.clone());
        path
    }
    /// The import path of the package in the resolved directory `dir`.
    fn directory(&mut self, dir: &Path) -> Option<String> {
        for base in dir.ancestors() {
            let module = match self
                .mods
                .entry(base.to_path_buf())
                .or_insert_with(|| GoMod::probe(base))
            {
                GoMod::Absent => continue,
                GoMod::Unusable => return None,
                GoMod::Module(module) => module,
            };
            let sub = crate::fsutil::rel_string(dir.strip_prefix(base).ok()?);
            return Some(if sub.is_empty() {
                module.clone()
            } else {
                format!("{module}/{sub}")
            });
        }
        None
    }
}
pub fn resolve_go_type_references<P: crate::extract::per_file::PerFile>(
    per_file: &[P],
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut [Value],
    root: &Path,
    context_nodes: &[Value],
    context_edges: &[Value],
) {
    let mut imports = BTreeMap::new();
    let mut actual = BTreeMap::new();
    for (p, r) in paths.iter().zip(per_file) {
        for n in r.nodes().into_iter().flatten() {
            let sf = crate::resolver_registry::rs(n, "source_file");
            if !sf.is_empty() {
                imports.insert(sf.to_string(), r.at("go_imports").clone());
                actual.insert(sf.to_string(), p.clone());
            }
        }
    }
    if imports.is_empty() {
        return;
    }
    let contained = edges
        .iter()
        .chain(context_edges)
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target").to_string())
        .collect::<BTreeSet<_>>();
    let mut fqns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut go_modules = GoModules::new(root);
    for n in nodes.iter().chain(context_nodes) {
        let sf = s(n, "source_file");
        if sf.is_empty()
            || s(n, "id").is_empty()
            || !contained.contains(s(n, "id"))
            || !is_type_like_definition(n)
        {
            continue;
        }
        if let Some(pkg) = go_modules.import_path(
            actual
                .get(sf)
                .map(PathBuf::as_path)
                .unwrap_or(Path::new(sf)),
        ) {
            fqns.entry(format!("{pkg}.{}", s(n, "label")))
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let stubs = nodes
        .iter()
        .filter(|n| s(n, "source_file").is_empty() && s(n, "label").contains('.'))
        .map(|n| (s(n, "id").to_string(), s(n, "label").to_string()))
        .collect::<BTreeMap<_, _>>();
    let mut ids = nodes
        .iter()
        .map(|n| s(n, "id").to_string())
        .collect::<BTreeSet<_>>();
    let mut added = vec![];
    let mut old = BTreeSet::new();
    for e in edges.iter_mut() {
        if !["references", "embeds"].contains(&s(e, "relation")) {
            continue;
        }
        let target = s(e, "target").to_string();
        let Some(label) = stubs.get(&target) else {
            continue;
        };
        let Some((alias, name)) = label.rsplit_once('.') else {
            continue;
        };
        let Some(pkg) = imports
            .get(s(e, "source_file"))
            .and_then(|m| m[alias].as_str())
        else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let fqn = format!("{pkg}.{name}");
        let candidates = fqns.get(&fqn).cloned().unwrap_or_default();
        let id = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            let id = make_id(["go", "type", fqn.as_str()]);
            if ids.insert(id.clone()) {
                added.push(json!({"id":id,"label":fqn,"file_type":"code","source_file":"","source_location":""}));
            }
            id
        };
        e["target"] = json!(id);
        old.insert(target);
    }
    nodes.extend(added);
    prune_repointed(nodes, edges, &old);
}
pub fn php_fqn_from_raw(raw: &str, ns: &str, uses: &BTreeMap<String, String>) -> String {
    let raw = raw.trim();
    if let Some(raw) = raw.strip_prefix('\\') {
        return raw.into();
    }
    if let Some((first, rest)) = raw.split_once('\\') {
        if let Some(mapped) = uses.get(&first.to_lowercase()) {
            return format!("{mapped}\\{rest}");
        }
    } else if let Some(mapped) = uses.get(&raw.to_lowercase()) {
        return mapped.clone();
    }
    if ns.is_empty() {
        raw.into()
    } else {
        format!("{ns}\\{raw}")
    }
}
/// What the PHP type pass reads from one file's tree: its namespace names,
/// its `use` imports (lowercased local name to FQN, first wins) and its class
/// heritage and trait names (kind and lowercased bare name to the raw name,
/// `None` once two spellings disagree). Kept by the file's extraction and by
/// the AST cache, like [`JavaSyntax`].
#[derive(Clone, Default)]
pub struct PhpSyntax {
    ns: Vec<String>,
    uses: BTreeMap<String, String>,
    raws: BTreeMap<(String, String), Option<String>>,
}
/// The kind under which [`PhpSyntax`] is kept in the run cache.
pub const PHP_SYNTAX: &str = "php_syntax";
type StoredPhpSyntax = (
    u32,
    Vec<String>,
    Vec<(String, String)>,
    Vec<(String, String, Option<String>)>,
);
impl PhpSyntax {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let stored: StoredPhpSyntax = (
            1,
            self.ns.clone(),
            self.uses
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            self.raws
                .iter()
                .map(|((rel, bare), raw)| (rel.clone(), bare.clone(), raw.clone()))
                .collect(),
        );
        serde_json::to_vec(&stored).ok()
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (format, ns, uses, raws): StoredPhpSyntax = serde_json::from_slice(bytes).ok()?;
        (format == 1).then(|| PhpSyntax {
            ns,
            uses: uses.into_iter().collect(),
            raws: raws
                .into_iter()
                .map(|(rel, bare, raw)| ((rel, bare), raw))
                .collect(),
        })
    }
}
pub fn php_syntax(parsed: &Parsed) -> PhpSyntax {
    let source = &parsed.source;
    let mut ns = vec![];
    let mut uses = BTreeMap::new();
    let mut raws: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
    for n in walk(parsed.tree.root_node()) {
        if n.kind_name() == "namespace_definition" {
            if let Some(name) = first_child(n, &["namespace_name"]) {
                ns.push(read_text(name, source));
            }
        } else if n.kind_name() == "namespace_use_declaration" {
            let prefix = first_child(n, &["namespace_name"])
                .map(|n| read_text(n, source))
                .unwrap_or_default();
            let mut clauses = children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "namespace_use_clause")
                .map(|c| (c, String::new()))
                .collect::<Vec<_>>();
            if let Some(group) = first_child(n, &["namespace_use_group"]) {
                clauses.extend(
                    children(group)
                        .into_iter()
                        .filter(|c| c.kind_name() == "namespace_use_clause")
                        .map(|c| (c, prefix.clone())),
                );
            }
            for (c, prefix) in clauses {
                if children(c)
                    .iter()
                    .any(|c| matches!(c.kind_name(), "function" | "const"))
                {
                    continue;
                }
                let mut target = None;
                let mut alias = None;
                let mut saw_as = false;
                for part in children(c) {
                    if part.kind_name() == "as" {
                        saw_as = true;
                    } else if matches!(part.kind_name(), "name" | "qualified_name") {
                        if saw_as {
                            alias = Some(read_text(part, source));
                        } else if target.is_none() {
                            target = Some(read_text(part, source));
                        }
                    }
                }
                if let Some(target) = target {
                    let fqn = if prefix.is_empty() {
                        target
                    } else {
                        format!("{prefix}\\{target}")
                    }
                    .trim_start_matches('\\')
                    .to_string();
                    let key = alias
                        .unwrap_or_else(|| fqn.rsplit('\\').next().unwrap_or("").into())
                        .trim()
                        .to_lowercase();
                    if !key.is_empty() {
                        uses.entry(key).or_insert(fqn);
                    }
                }
            }
        } else if n.kind_name() == "class_declaration" {
            let mut bases = vec![];
            for c in children(n) {
                if ["base_clause", "class_interface_clause"].contains(&c.kind_name()) {
                    let relation = if c.kind_name() == "base_clause" {
                        "inherits"
                    } else {
                        "implements"
                    };
                    bases.extend(
                        children(c)
                            .into_iter()
                            .filter(|n| matches!(n.kind_name(), "name" | "qualified_name"))
                            .map(|n| (relation, n)),
                    );
                } else if c.kind_name() == "declaration_list" {
                    for m in children(c)
                        .into_iter()
                        .filter(|c| c.kind_name() == "use_declaration")
                    {
                        bases.extend(
                            children(m)
                                .into_iter()
                                .filter(|n| matches!(n.kind_name(), "name" | "qualified_name"))
                                .map(|n| ("mixes_in", n)),
                        );
                    }
                }
            }
            for (rel, n) in bases {
                let raw = read_text(n, source);
                let bare = raw.rsplit('\\').next().unwrap_or("").trim().to_lowercase();
                if bare.is_empty() {
                    continue;
                }
                let key = (rel.to_string(), bare);
                if let Some(value) = raws.get_mut(&key) {
                    if value.as_deref() != Some(&raw) {
                        *value = None;
                    }
                } else {
                    raws.insert(key, Some(raw));
                }
            }
        }
    }
    PhpSyntax { ns, uses, raws }
}
/// Each PHP file's [`PhpSyntax`], kept or parsed here, files in parallel.
fn php_syntaxes(paths: &[PathBuf]) -> Vec<Option<PhpSyntax>> {
    parse_cache::par_map(paths, |path| {
        parse_cache::take_facts(PHP_SYNTAX, path)
            .and_then(|f| f.downcast::<PhpSyntax>().ok())
            .map(|f| *f)
            .or_else(|| {
                parse(path, "php", graphicle_tree_sitter_php::LANGUAGE_PHP.into())
                    .map(|parsed| php_syntax(&parsed))
            })
    })
}
pub fn resolve_php_type_references(
    per_file: &[Value],
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut [Value],
) {
    let mut namespaces = BTreeMap::new();
    let mut uses_by_file: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut raw_by_file = BTreeMap::new();
    for (result, syntax) in per_file.iter().zip(php_syntaxes(paths)) {
        let Some(PhpSyntax { ns, uses, raws }) = syntax else {
            continue;
        };
        if ns.iter().collect::<BTreeSet<_>>().len() > 1 {
            continue;
        }
        let ns = ns.first().cloned().unwrap_or_default();
        for n in result["nodes"].as_array().into_iter().flatten() {
            let sf = s(n, "source_file");
            if !sf.is_empty() {
                namespaces.insert(sf.to_string(), ns.clone());
                uses_by_file.insert(sf.to_string(), uses.clone());
                raw_by_file.insert(sf.to_string(), raws.clone());
            }
        }
    }
    let mut fqns = BTreeMap::new();
    for n in nodes.iter() {
        let (label, sf, id) = (s(n, "label"), s(n, "source_file"), s(n, "id"));
        if label.is_empty() || label.ends_with(')') || label.contains('.') || id.is_empty() {
            continue;
        }
        if let Some(ns) = namespaces.get(sf) {
            let fqn = if ns.is_empty() {
                label.into()
            } else {
                format!("{ns}\\{label}")
            };
            fqns.entry(fqn.to_lowercase()).or_insert(id.to_string());
        }
    }
    let stubs = nodes
        .iter()
        .filter(|n| s(n, "source_file").is_empty() && !s(n, "label").is_empty())
        .map(|n| (s(n, "id").to_string(), s(n, "label").to_string()))
        .collect::<BTreeMap<_, _>>();
    let mut ids = nodes
        .iter()
        .map(|n| s(n, "id").to_string())
        .collect::<BTreeSet<_>>();
    let mut old = BTreeSet::new();
    let mut added = vec![];
    let mut external = BTreeMap::new();
    for e in edges.iter_mut() {
        let rel = s(e, "relation");
        if ![
            "inherits",
            "implements",
            "mixes_in",
            "imports",
            "references",
        ]
        .contains(&rel)
        {
            continue;
        }
        let sf = s(e, "source_file");
        let Some(ns) = namespaces.get(sf) else {
            continue;
        };
        let uses = &uses_by_file[sf];
        let target = s(e, "target").to_string();
        let label = stubs.get(&target).cloned().or_else(|| {
            if rel == "imports" {
                uses.keys()
                    .find(|k| make_id([k.as_str()]) == target)
                    .cloned()
            } else {
                None
            }
        });
        let Some(label) = label else { continue };
        let bare = label.trim().to_lowercase();
        let raw = if ["inherits", "implements", "mixes_in"].contains(&rel) {
            raw_by_file
                .get(sf)
                .and_then(|m| m.get(&(rel.to_string(), bare.clone())))
                .cloned()
                .flatten()
        } else {
            None
        };
        let (fqn, explicit) = if let Some(raw) = raw.filter(|r| r.contains('\\')) {
            (php_fqn_from_raw(&raw, ns, uses), true)
        } else if let Some(fqn) = uses.get(&bare) {
            (fqn.clone(), true)
        } else if !ns.is_empty() {
            (format!("{ns}\\{label}"), false)
        } else {
            continue;
        };
        let mut bound = fqns.get(&fqn.to_lowercase()).cloned();
        if explicit && bound.is_none() {
            let id = external
                .entry(fqn.to_lowercase())
                .or_insert_with(|| make_id([fqn.as_str()]))
                .clone();
            if ids.insert(id.clone()) {
                added.push(json!({"id":id,"label":fqn,"file_type":"code","source_file":"","source_location":""}));
            }
            bound = Some(id);
        }
        if let Some(id) = bound.filter(|id| *id != target) {
            e["target"] = json!(id);
            old.insert(target);
        }
    }
    nodes.extend(added);
    prune_repointed(nodes, edges, &old);
}

#[cfg(test)]
mod python_visit_tests {
    use super::*;

    /// The recorded-then-replayed walk answers exactly what `visit` does.
    #[test]
    fn replay_matches_the_tree_walk() {
        let sources = [
            "from pkg.mod import Alpha, beta as b\nfrom . import sib\nfrom ..up import Gamma\n\
             class Outer:\n    def method(self, x):\n        return Alpha(x) + b(x) + Gamma\n\
             \n    class Inner:\n        def deep(self):\n            return sib.run(Alpha)\n\
             \ndef free(y):\n    def helper():\n        return beta(y)\n    return helper() + Alpha\n",
            "import os\nfrom amb import Thing\nfrom amb.sub import Other\nfrom pkg import (Alpha,\n    Delta as D)\n\
             @decorate(Alpha)\ndef top(a=Alpha, *args, **kw):\n    lambda q: q + Delta\n    from pkg.mod import Late\n    return Late, D\n\
             def unnamed_owner():\n    class Unlisted:\n        def inside(self):\n            return Alpha\n    return Unlisted\n",
            "def top():\n    x = 1\n    x = x + 2\n\ndef top():\n    return x\n\nclass K(Base, metaclass=M):\n    attr: Alpha = Alpha()\n",
            "from .....far import Z\nfrom .rel.mod import Y\nfrom x import *\n\ndef f():\n    return Z(Y)\n",
            "",
            "def broken(:\n    return Alpha\nclass\n",
        ];
        let mut entities = BTreeMap::new();
        for (stem, labels) in [
            (
                "pkg/mod",
                vec![
                    ("Alpha", "mod_alpha"),
                    ("beta", "mod_beta"),
                    ("Late", "mod_late"),
                ],
            ),
            (
                "src/pkg/__init__",
                vec![("Alpha", "pkg_alpha"), ("Delta", "pkg_delta")],
            ),
            ("lib/pkg", vec![("Alpha", "lib_alpha")]),
            ("a/up", vec![("Gamma", "up_gamma")]),
            ("a/b/sib", vec![("run", "sib_run")]),
            ("amb", vec![("Thing", "amb_thing")]),
            ("a/b/rel/mod", vec![("Y", "rel_y")]),
        ] {
            entities.insert(
                stem.to_string(),
                labels
                    .into_iter()
                    .map(|(l, i)| (l.to_string(), i.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            );
        }
        let bare = BTreeMap::from([
            ("mod".to_string(), "pkg/mod".to_string()),
            ("pkg".to_string(), "lib/pkg".to_string()),
        ]);
        let ambiguous = super::super::resolution_facts::AmbiguousModules::from_iter([
            make_id(["amb"]),
            make_id(["pkg.Delta"]),
        ]);
        let tails = tail_index(&entities);
        let path = Path::new("a/b/c.py");
        let name_sets = [
            vec!["Outer", "free", "top", "K", "f", "broken"],
            vec!["Inner", "helper", "method", "inside"],
            vec!["deep", "top", "unnamed_owner", "Unlisted"],
            vec![],
        ];
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        for source in sources {
            let tree = parser.parse(source, None).unwrap();
            let facts = record_python_visit(tree.root_node(), source.as_bytes());
            for labels in &name_sets {
                let names = labels
                    .iter()
                    .map(|l| (l.to_string(), format!("id_{l}")))
                    .collect::<BTreeMap<_, _>>();
                for own_stems in [BTreeSet::new(), BTreeSet::from(["pkg/mod".to_string()])] {
                    let (mut imports, mut uses) = (BTreeMap::new(), BTreeMap::new());
                    visit(
                        tree.root_node(),
                        None,
                        PythonResolutionContext {
                            path,
                            source: source.as_bytes(),
                            names: &names,
                            entities: &entities,
                            tails: &tails,
                            bare: &bare,
                            imports: &mut imports,
                            uses: &mut uses,
                            ambiguous: &ambiguous,
                            own_stems: &own_stems,
                        },
                    );
                    let replayed = replay_python_visit(
                        &facts, path, &names, &entities, &tails, &bare, &ambiguous, &own_stems,
                    );
                    assert_eq!(replayed, (imports, uses), "{source:?} with {labels:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod python_visit_corpus_tests {
    use super::*;

    /// `GRAPHICLE_VISIT_CORPUS=<dir> cargo test --lib -- --ignored visit_corpus`:
    /// the replay against `visit` on every Python file under a checkout, with
    /// the corpus's own modules as entities and every second definition name
    /// of a file as its owners.
    #[test]
    #[ignore]
    fn visit_corpus() {
        let Ok(dir) = std::env::var("GRAPHICLE_VISIT_CORPUS") else {
            return;
        };
        let mut files = vec![];
        let mut stack = vec![PathBuf::from(dir)];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "py") {
                    files.push(p);
                }
            }
        }
        files.sort();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let parsed = files
            .iter()
            .filter_map(|p| {
                let source = std::fs::read(p).ok()?;
                let tree = parser.parse(&source, None)?;
                Some((p.clone(), source, tree))
            })
            .collect::<Vec<_>>();
        let mut entities: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut bare = BTreeMap::new();
        let mut definitions = vec![];
        for (p, source, tree) in &parsed {
            let stem = file_stem(p);
            let mut names = vec![];
            for n in walk(tree.root_node()) {
                if matches!(n.kind(), "class_definition" | "function_definition")
                    && let Some(name) = n.child_by_field_name("name")
                {
                    let name = read_text(name, source);
                    entities
                        .entry(stem.clone())
                        .or_default()
                        .insert(name.clone(), format!("{stem}::{name}"));
                    names.push(name);
                }
            }
            bare.entry(
                p.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            )
            .or_insert(stem);
            definitions.push(names);
        }
        let tails = tail_index(&entities);
        let ambiguous = super::super::resolution_facts::AmbiguousModules::default();
        let mut compared = 0;
        for ((p, source, tree), defs) in parsed.iter().zip(&definitions) {
            let facts = record_python_visit(tree.root_node(), source);
            for skip in [0, 1] {
                let names = defs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| i % 2 == skip)
                    .map(|(_, l)| (l.clone(), format!("owner_{l}")))
                    .collect::<BTreeMap<_, _>>();
                let (mut imports, mut uses) = (BTreeMap::new(), BTreeMap::new());
                let own_stems = BTreeSet::from([file_stem(p)]);
                visit(
                    tree.root_node(),
                    None,
                    PythonResolutionContext {
                        path: p,
                        source,
                        names: &names,
                        entities: &entities,
                        tails: &tails,
                        bare: &bare,
                        imports: &mut imports,
                        uses: &mut uses,
                        ambiguous: &ambiguous,
                        own_stems: &own_stems,
                    },
                );
                let replayed = replay_python_visit(
                    &facts, p, &names, &entities, &tails, &bare, &ambiguous, &own_stems,
                );
                assert_eq!(replayed, (imports, uses), "{}", p.display());
                compared += 1;
            }
        }
        eprintln!(
            "visit_corpus: {compared} comparisons over {} files",
            parsed.len()
        );
    }
}

#[cfg(test)]
mod python_syntax_tests {
    use super::*;
    use crate::extractors::resolution_facts::{
        call_identifier, python_import_from_module, python_imported_names, top_bodies,
    };

    /// The separate walks `record_python_syntax` replaces.
    fn reference(path: &Path, root: Node<'_>, s: &[u8]) -> (PythonImportFroms, PythonBodyCalls) {
        let mut imports = vec![];
        for n in walk(root)
            .into_iter()
            .filter(|n| n.kind() == "import_from_statement")
        {
            if let Some((level, module)) = python_import_from_module(n, s) {
                imports.push((
                    level,
                    module,
                    (n.start_position().row + 1) as i64,
                    python_imported_names(n, s),
                ));
            }
        }
        let mut calls = vec![];
        for (id, b) in top_bodies(path, root, s, true) {
            for n in walk(b) {
                if let Some(name) = call_identifier(n, s, true) {
                    calls.push((id.clone(), name, (n.start_position().row + 1) as i64));
                }
            }
        }
        (imports, calls)
    }

    fn check(path: &Path, source: &[u8], parser: &mut tree_sitter::Parser) {
        let Some(tree) = parser.parse(source, None) else {
            return;
        };
        let (_, imports, calls) = record_python_syntax(Some(path), tree.root_node(), source);
        assert_eq!(
            (imports, calls),
            reference(path, tree.root_node(), source),
            "{}",
            path.display()
        );
    }

    #[test]
    fn one_walk_matches_the_separate_walks() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let sources = [
            "from a.b import c, d as e\nfrom . import f\nfrom .. import *\nimport os\n\
             def top(x):\n    from .inner import g\n    return g(x) + len(x)\n\
             @decorated\ndef skipped():\n    return call_me()\n\
             class K:\n    def m(self):\n        return helper()\n\
             def outer():\n    def nested():\n        return deep()\n    return nested()\n",
            "if True:\n    from z import y\n\ndef f(): pass\ndef g():\n    try:\n        h()\n    except E:\n        i()\n",
            "def broken(:\n    from x import\n    call()\n",
            "",
        ];
        for source in sources {
            check(Path::new("pkg/mod.py"), source.as_bytes(), &mut parser);
        }
        if let Ok(dir) = std::env::var("GRAPHICLE_VISIT_CORPUS") {
            let mut stack = vec![PathBuf::from(dir)];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|e| e == "py")
                        && let Ok(source) = std::fs::read(&p)
                    {
                        check(&p, &source, &mut parser);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod go_module_tests {
    use super::*;
    use std::fs;

    /// The lookup before `GoModules`: every call resolves the path, probes
    /// each parent directory and reads `go.mod` again.
    fn uncached(source: &Path, root: &Path) -> Option<String> {
        let module = regex::Regex::new(r"(?m)^\s*module\s+([^\s]+)").unwrap();
        let p = if source.is_absolute() {
            source.to_path_buf()
        } else {
            root.join(source)
        };
        let p = resolved(&p);
        let dir = p.parent()?;
        for base in dir.ancestors() {
            let gm = base.join("go.mod");
            if !gm.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(gm).ok()?;
            let cap = module.captures(&raw)?;
            let sub = dir.strip_prefix(base).ok()?.to_string_lossy();
            return Some(if sub.is_empty() {
                cap[1].into()
            } else {
                format!("{}/{sub}", &cap[1])
            });
        }
        None
    }

    /// `module` plus the package directory below it, as the lookup spells
    /// it on this platform.
    fn import(module: &str, parts: &[&str]) -> Option<String> {
        let sub = parts.iter().collect::<PathBuf>();
        Some(if parts.is_empty() {
            module.into()
        } else {
            format!("{module}/{}", sub.to_string_lossy())
        })
    }

    fn write(path: &Path, body: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A tree shaped like kubernetes: a root module, nested modules under
    /// staging/ and a vendor/ symlink onto one, plus a go.mod that cannot be
    /// read, one without a module line and a directory named go.mod.
    fn layout() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work").join("kubernetes");
        let staging = root.join("staging/src/k8s.io");
        write(
            &root.join("go.mod"),
            b"module k8s.io/kubernetes\n\ngo 1.25.0\n",
        );
        for file in [
            "cmd/kubelet/app/server.go",
            "pkg/api/v1/types.go",
            "main.go",
        ] {
            write(&root.join(file), b"package x\n");
        }
        write(
            &staging.join("api/go.mod"),
            b"// This is a generated file. Do not edit directly.\n\nmodule k8s.io/api\n\ngo 1.25.0\n",
        );
        write(&staging.join("api/doc.go"), b"package api\n");
        write(&staging.join("api/core/v1/types.go"), b"package v1\n");
        write(
            &staging.join("client-go/go.mod"),
            b"\t module   k8s.io/client-go // comment\n",
        );
        write(
            &staging.join("client-go/tools/cache/store.go"),
            b"package cache\n",
        );
        // Not UTF-8, so it cannot be read: the search ends with no path.
        write(&staging.join("broken/go.mod"), b"module k8s.io/\xff\xfe\n");
        write(&staging.join("broken/sub/x.go"), b"package sub\n");
        // No module line: the search ends with no path too.
        write(&staging.join("nomodule/go.mod"), b"go 1.25.0\n");
        write(&staging.join("nomodule/y.go"), b"package nomodule\n");
        // A directory named go.mod is no go.mod file: the search goes on up.
        fs::create_dir_all(staging.join("dirmod/go.mod")).unwrap();
        write(&staging.join("dirmod/z.go"), b"package dirmod\n");
        write(&tmp.path().join("elsewhere/w.go"), b"package w\n");
        #[cfg(unix)]
        {
            fs::create_dir_all(root.join("vendor/k8s.io")).unwrap();
            std::os::unix::fs::symlink(
                "../../staging/src/k8s.io/api",
                root.join("vendor/k8s.io/api"),
            )
            .unwrap();
        }
        (tmp, root)
    }

    fn sources(root: &Path) -> Vec<PathBuf> {
        let relative = [
            "cmd/kubelet/app/server.go",
            "pkg/api/v1/types.go",
            "main.go",
            "go.mod",
            "staging/src/k8s.io/api/doc.go",
            "staging/src/k8s.io/api/core/v1/types.go",
            "staging/src/k8s.io/api/core/v1/missing.go",
            "staging/src/k8s.io/api",
            "staging/src/k8s.io/client-go/tools/cache/store.go",
            "staging/src/k8s.io/broken/sub/x.go",
            "staging/src/k8s.io/broken/go.mod",
            "staging/src/k8s.io/nomodule/y.go",
            "staging/src/k8s.io/dirmod/z.go",
            "vendor/k8s.io/api/core/v1/types.go",
            "staging/src/k8s.io/../k8s.io/api/doc.go",
            // The same Path as earlier entries, spelled with other bytes.
            "staging/src/k8s.io/api//core/v1/types.go",
            "staging/src/k8s.io/api/./core/v1/types.go",
            "staging/src/k8s.io/api/core/v1/",
            "../../elsewhere/w.go",
            "",
        ]
        .map(PathBuf::from);
        let absolute = relative
            .iter()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| root.join(p));
        relative.iter().cloned().chain(absolute).collect()
    }

    #[test]
    fn cached_lookup_matches_the_uncached_one_on_nested_modules() {
        let (_tmp, root) = layout();
        let all = sources(&root);
        // One cache across three passes over the files, in both orders.
        let mut shared = GoModules::new(&root);
        for pass in 0..3 {
            let order: Vec<&PathBuf> = if pass == 1 {
                all.iter().rev().collect()
            } else {
                all.iter().collect()
            };
            for source in order {
                let want = uncached(source, &root);
                let shown = source.display();
                assert_eq!(go_import_path_for_file(source, &root), want, "{shown}");
                assert_eq!(
                    shared.import_path(source).map(str::to_owned),
                    want,
                    "{shown}"
                );
            }
        }
        let path = |s: &str| go_import_path_for_file(Path::new(s), &root);
        let kubernetes = "k8s.io/kubernetes";
        assert_eq!(
            path("cmd/kubelet/app/server.go"),
            import(kubernetes, &["cmd", "kubelet", "app"])
        );
        assert_eq!(path("main.go"), import(kubernetes, &[]));
        assert_eq!(path("go.mod"), import(kubernetes, &[]));
        assert_eq!(
            path("staging/src/k8s.io/api/doc.go"),
            import("k8s.io/api", &[])
        );
        assert_eq!(
            path("staging/src/k8s.io/api/core/v1/types.go"),
            import("k8s.io/api", &["core", "v1"])
        );
        assert_eq!(
            path("staging/src/k8s.io/api/core/v1/missing.go"),
            import("k8s.io/api", &["core", "v1"])
        );
        assert_eq!(
            path("staging/src/k8s.io/client-go/tools/cache/store.go"),
            import("k8s.io/client-go", &["tools", "cache"])
        );
        assert_eq!(path("staging/src/k8s.io/broken/sub/x.go"), None);
        assert_eq!(path("staging/src/k8s.io/broken/go.mod"), None);
        assert_eq!(path("staging/src/k8s.io/nomodule/y.go"), None);
        assert_eq!(
            path("staging/src/k8s.io/dirmod/z.go"),
            import(kubernetes, &["staging", "src", "k8s.io", "dirmod"])
        );
        #[cfg(unix)]
        assert_eq!(
            path("vendor/k8s.io/api/core/v1/types.go"),
            import("k8s.io/api", &["core", "v1"])
        );
    }

    /// A cache answers from what it read; the next pass reads the files again,
    /// as an update or a watch rebuild must.
    #[test]
    fn a_new_pass_reads_go_mod_again() {
        let (_tmp, root) = layout();
        let types = Path::new("staging/src/k8s.io/api/core/v1/types.go");
        let doc = Path::new("staging/src/k8s.io/api/doc.go");
        let y = Path::new("staging/src/k8s.io/nomodule/y.go");
        let mut pass = GoModules::new(&root);
        assert_eq!(
            pass.import_path(types).map(str::to_owned),
            import("k8s.io/api", &["core", "v1"])
        );
        assert_eq!(pass.import_path(y), None);
        fs::write(
            root.join("staging/src/k8s.io/api/go.mod"),
            "module k8s.io/api/v2\n",
        )
        .unwrap();
        fs::remove_file(root.join("staging/src/k8s.io/nomodule/go.mod")).unwrap();
        // Each go.mod is read once per pass, also for a file not asked before.
        assert_eq!(
            pass.import_path(types).map(str::to_owned),
            import("k8s.io/api", &["core", "v1"])
        );
        assert_eq!(pass.import_path(doc), Some("k8s.io/api"));
        assert_eq!(pass.import_path(y), None);
        let mut next = GoModules::new(&root);
        assert_eq!(
            next.import_path(types).map(str::to_owned),
            import("k8s.io/api/v2", &["core", "v1"])
        );
        assert_eq!(next.import_path(doc), Some("k8s.io/api/v2"));
        assert_eq!(
            next.import_path(y).map(str::to_owned),
            import(
                "k8s.io/kubernetes",
                &["staging", "src", "k8s.io", "nomodule"]
            )
        );
        for source in [types, doc, y] {
            assert_eq!(
                go_import_path_for_file(source, &root),
                uncached(source, &root)
            );
        }
    }
}
#[cfg(test)]
mod java_pass_tests {
    use crate::extract::{ExtractOptions, extract};
    use serde_json::Value;

    /// The Java type and import passes read the per-file results in place:
    /// over compact records they give byte for byte the extraction they give
    /// over JSON (the form they read before), cold and from the cache.
    #[test]
    fn java_passes_on_records_extract_exactly_like_json() {
        let _serial = crate::extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let mut paths = vec![
            write(
                "j/p/Base.java",
                "package p;\n\npublic class Base {\n    public static class Inner {\n        public static class Leaf {}\n    }\n    public static void helper() {}\n}\n",
            ),
            write("j/p/Api.java", "package p;\n\npublic interface Api {}\n"),
            write(
                "j/q/Impl.java",
                "package q;\n\nimport p.Base;\nimport p.Api;\nimport p.Base.Inner;\nimport org.lib.Missing;\nimport org.lib.Outer.Gone;\nimport static p.Base.helper;\nimport java.util.*;\n\n@Deprecated\npublic class Impl extends Base implements Api, Missing {\n    @Deprecated Inner inner;\n    @Deprecated Missing missing;\n    Local local;\n    Gone gone;\n    @Override public String toString() { return \"\"; }\n    @Override public int hashCode() { return 1; }\n}\n",
            ),
            write(
                "j/q/Local.java",
                "package q;\n\nclass Local extends Impl {}\n",
            ),
            write(
                "j/r/Nested.java",
                "package r;\n\nimport p.Base.Inner;\nimport p.Base.Inner.Leaf;\n\npublic class Nested {\n    class Deep {\n        class Deeper extends Inner implements p.Api {}\n        class Deepest extends Leaf {}\n    }\n}\n",
            ),
            write("j/Plain.java", "public class Plain extends Nested {}\n"),
        ];
        paths.sort();
        let run = |json_only: bool, use_cache: bool| {
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(json_only));
            let out = extract(
                &paths,
                &ExtractOptions {
                    root: Some(root.clone()),
                    cache_root: Some(root.join(if json_only { "cache-json" } else { "cache-rec" })),
                    use_cache,
                    parallel: false,
                    ..Default::default()
                },
            );
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(false));
            serde_json::to_string(&out.unwrap()).unwrap()
        };
        for use_cache in [false, true, true] {
            assert_eq!(
                run(false, use_cache),
                run(true, use_cache),
                "cache {use_cache}"
            );
        }
        // The fixture reaches both passes: a file's import bound to the class
        // it names in another file.
        let out: Value = serde_json::from_str(&run(false, false)).unwrap();
        let file_of = |id: &Value| {
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == *id)
                .and_then(|n| n["source_file"].as_str())
                .unwrap_or("")
                .to_string()
        };
        assert!(
            out["edges"].as_array().unwrap().iter().any(|e| {
                e["relation"] == "imports"
                    && e["source_file"]
                        .as_str()
                        .is_some_and(|f| f.ends_with("Impl.java"))
                    && file_of(&e["target"]).ends_with("Base.java")
            }),
            "no import bound across files"
        );
    }
}
