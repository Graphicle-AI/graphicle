// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Corpus normalization, collision-safe merges, and import alias repair.
use crate::{
    build::truthy,
    extract::per_file::PerFile,
    extractors::{
        base::{file_stem, is_language_builtin, make_id},
        csharp::{is_cs_file, is_dotnet_source_file},
        resolution::{is_python_stdlib_module, is_type_like_definition, resolved, s},
        resolution_paths::is_python_package_dir,
    },
    python_compat::string,
    rcfile::effective_suffix,
    resolver_registry::{Rec, rs},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub static CONSTANTS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/extract/constants.json")).unwrap());
/// `_lang_family`: the interop family of the file's language, by its
/// effective suffix (a `.graphiclerc` declaration counts, #2961).
pub fn family(source: &str) -> Option<&'static str> {
    if source.is_empty() {
        return None;
    }
    CONSTANTS["_LANG_FAMILY_BY_EXT"][effective_suffix(source).to_lowercase()].as_str()
}
/// `_lang_is_case_insensitive`, by the effective suffix.
pub fn case_insensitive(source: &str) -> bool {
    if source.is_empty() {
        return false;
    }
    let ext = effective_suffix(source).to_lowercase();
    CONSTANTS["_CASE_INSENSITIVE_EXTS"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == &ext)
}
/// `re.sub(r"[^a-zA-Z0-9]+", "", label)`, the member resolvers' key.
pub fn label_key(label: &str, fold: bool) -> String {
    let key = label
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    if fold { key.to_lowercase() } else { key }
}
/// `_node_label_key`: the stub-rewire key. Underscores are identifier
/// characters, so a private `_Response` never shares a key with `Response`
/// (#4269); only call/generic punctuation and whitespace drop out.
pub fn node_label_key(label: &str, fold: bool) -> String {
    let key = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>();
    if fold { key.to_lowercase() } else { key }
}
/// Python's `value == 1` for a JSON scalar (`True` and `1.0` included).
fn equals_one(value: &Value) -> bool {
    *value == Value::Bool(true) || value.as_f64() == Some(1.0)
}
pub fn top_function(n: &Value) -> bool {
    let l = s(n, "label").trim();
    s(n, "file_type") == "code" && l.ends_with(')') && !l.starts_with('.') && !l.contains('.')
}
pub fn file_node_id(p: &Path) -> String {
    make_id([file_stem(p).as_str()])
}
pub fn remap_edges(edges: &mut [Value], mapping: &BTreeMap<String, String>) {
    for e in edges {
        for key in ["source", "target"] {
            if let Some(id) = mapping.get(s(e, key)) {
                e[key] = json!(id);
            }
        }
    }
}
pub fn remap_objc_fields<P: PerFile>(per: &mut [P], mapping: &BTreeMap<String, String>) {
    for r in per {
        if let Some(tables) = r.at("objc_field_types")["tables"].as_object() {
            let new = tables
                .iter()
                .map(|(k, v)| (mapping.get(k).unwrap_or(k).clone(), v.clone()))
                .collect::<serde_json::Map<_, _>>();
            r.key_mut("objc_field_types").expect("read just above")["tables"] = json!(new);
        }
    }
}
pub fn canonicalize_namespaces(nodes: &mut Vec<Value>, edges: &mut [Value]) {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if s(n, "type") == "namespace" && n["label"].is_string() {
            groups.entry(s(n, "label").into()).or_default().push(i);
        }
    }
    let mut drops = BTreeSet::new();
    let mut remap = BTreeMap::new();
    for group in groups.values_mut().filter(|g| g.len() > 1) {
        group.sort_by_key(|i| {
            (
                s(&nodes[*i], "source_file"),
                s(&nodes[*i], "source_location"),
                s(&nodes[*i], "id"),
            )
        });
        for i in &group[1..] {
            drops.insert(*i);
            remap.insert(
                s(&nodes[*i], "id").to_string(),
                s(&nodes[group[0]], "id").to_string(),
            );
        }
    }
    remap_edges(edges, &remap);
    let mut i = 0;
    nodes.retain(|_| {
        let keep = !drops.contains(&i);
        i += 1;
        keep
    });
}
/// `_rewire_unique_stub_nodes`: map each sourceless stub to the unique real
/// definition with its label, rewire the edges, and drop the stubs no edge
/// names any more.
pub fn rewire_unique_stubs(nodes: &mut Vec<Value>, edges: &mut [Value]) {
    rewire_unique_stubs_with(nodes, &[], edges);
}
/// [`rewire_unique_stubs`] over `nodes` followed by `lent` candidates that
/// are never kept: the unchanged files' nodes of an incremental build, and
/// a stub for each Rust `use` of this crate whose name nothing declares
/// (#4439). `nodes` keeps exactly what the rewire over the whole view keeps.
pub fn rewire_unique_stubs_with(nodes: &mut Vec<Value>, lent: &[&Value], edges: &mut [Value]) {
    let map = rewire_unique_stub_edges_with(nodes, lent, edges);
    if map.is_empty() {
        return;
    }
    let refs = edges
        .iter()
        .flat_map(|e| [s(e, "source"), s(e, "target")])
        .collect::<FxHashSet<_>>();
    nodes.retain(|n| !map.contains_key(s(n, "id")) || refs.contains(s(n, "id")));
}
/// The edge half of [`rewire_unique_stubs`]: `nodes` stay as they are (the
/// Python member resolver runs it on copies of the `inherits` edges). Returns
/// the stub -> definition map.
pub fn rewire_unique_stub_edges(nodes: &[Value], edges: &mut [Value]) -> FxHashMap<String, String> {
    rewire_unique_stub_edges_with(nodes, &[], edges)
}
fn rewire_unique_stub_edges_with(
    nodes: &[Value],
    lent: &[&Value],
    edges: &mut [Value],
) -> FxHashMap<String, String> {
    let view = || nodes.iter().chain(lent.iter().copied());
    // Every table here is only looked up, never iterated, so hash tables give
    // the same result as ordered maps without the string comparisons.
    let mut defs: FxHashMap<String, Vec<&Value>> = FxHashMap::default();
    let mut ci: FxHashMap<String, Vec<&Value>> = FxHashMap::default();
    let mut funcs: FxHashMap<String, Vec<&Value>> = FxHashMap::default();
    let mut stubs = vec![];
    for n in view() {
        let key = node_label_key(s(n, "label"), false);
        if key.is_empty() {
            continue;
        }
        if !s(n, "source_file").is_empty() {
            if is_type_like_definition(n) {
                defs.entry(key).or_default().push(n);
                if case_insensitive(s(n, "source_file")) {
                    ci.entry(node_label_key(s(n, "label"), true))
                        .or_default()
                        .push(n);
                }
            } else if top_function(n) {
                funcs.entry(key).or_default().push(n);
            }
        } else {
            stubs.push(n);
        }
    }
    let stub_ids = stubs.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
    let mut fams: FxHashMap<String, BTreeSet<&str>> = FxHashMap::default();
    let mut supers = FxHashSet::default();
    for e in edges.iter() {
        for endpoint in ["source", "target"] {
            let id = s(e, endpoint);
            if stub_ids.contains(id) {
                if let Some(f) = family(s(e, "source_file")) {
                    fams.entry(id.into()).or_default().insert(f);
                }
                if endpoint == "target"
                    && ["inherits", "implements", "extends"].contains(&s(e, "relation"))
                {
                    supers.insert(id);
                }
            }
        }
    }
    let empty = vec![];
    let mut map = FxHashMap::default();
    for n in stubs {
        let id = s(n, "id");
        if id.is_empty() {
            continue;
        }
        let mut candidates = defs
            .get(&node_label_key(s(n, "label"), false))
            .unwrap_or(&empty)
            .clone();
        // #4283: a Rust type with `impl` blocks in other files has one node
        // per file, but only its declaration carries `_rust_declaration_count`.
        // Exactly one declaration in total is one type; anything else (or a
        // same-named candidate in another language) keeps the stub.
        if candidates.len() > 1
            && candidates
                .iter()
                .all(|c| family(s(c, "source_file")) == Some("rust"))
        {
            let declared = candidates
                .iter()
                .filter(|c| c.get("_rust_declaration_count").is_some_and(truthy))
                .copied()
                .collect::<Vec<_>>();
            if declared.len() == 1 && equals_one(&declared[0]["_rust_declaration_count"]) {
                candidates = declared;
            }
        }
        if candidates.len() != 1 {
            candidates = ci
                .get(&node_label_key(s(n, "label"), true))
                .unwrap_or(&empty)
                .clone();
        }
        if candidates.len() != 1 {
            let f = funcs
                .get(&node_label_key(s(n, "label"), false))
                .unwrap_or(&empty);
            if f.len() == 1 && !supers.contains(id) {
                let allowed = fams.get(id);
                if allowed.is_none_or(BTreeSet::is_empty)
                    || family(s(f[0], "source_file"))
                        .is_none_or(|f| allowed.is_some_and(|a| a.contains(f)))
                {
                    candidates = f.clone();
                }
            }
        }
        if candidates.len() == 1 {
            let target = s(candidates[0], "id");
            if !target.is_empty() && target != id {
                map.insert(id.to_string(), target.to_string());
            }
        }
    }
    let by_id = view().map(|n| (s(n, "id"), n)).collect::<FxHashMap<_, _>>();
    for e in edges.iter_mut() {
        let sf = s(e, "source_file").to_string();
        let scoped = is_dotnet_source_file(&e["source_file"])
            && ["inherits", "implements", "references", "imports"].contains(&s(e, "relation"));
        for endpoint in ["source", "target"] {
            let old = s(e, endpoint);
            let Some(new) = map.get(old) else { continue };
            let target = by_id.get(new.as_str());
            if scoped && target.is_some_and(|n| is_cs_file(&n["source_file"])) {
                continue;
            }
            if endpoint == "target"
                && ["inherits", "implements", "extends"].contains(&s(e, "relation"))
                && let Some(f) = family(&sf)
            {
                let label = by_id.get(old).map(|n| s(n, "label").trim()).unwrap_or("");
                let builtin = CONSTANTS["_LANGUAGE_BUILTIN_BASE_CLASSES"][f]
                    .as_array()
                    .is_some_and(|a| {
                        a.iter().any(|v| {
                            if case_insensitive(&sf) {
                                v.as_str().unwrap_or("").eq_ignore_ascii_case(label)
                            } else {
                                v == label
                            }
                        })
                    });
                if builtin
                    && target
                        .and_then(|n| family(s(n, "source_file")))
                        .is_some_and(|g| g != f)
                {
                    continue;
                }
            }
            // Graphify 0.9.76 (#2207): per edge, a unique candidate in another
            // language family is refused, so a missing base stays on its stub;
            // a same-family referrer of the same stub still rewires.
            if let (Some(edge_family), Some(candidate_family)) = (
                family(&sf),
                target.and_then(|n| family(s(n, "source_file"))),
            ) && edge_family != candidate_family
            {
                continue;
            }
            e[endpoint] = json!(new);
        }
    }
    map
}
pub fn merge_swift_extensions<P: PerFile>(
    per: &[P],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    let mut ext = BTreeMap::new();
    for r in per {
        for e in r.at("swift_extensions").as_array().into_iter().flatten() {
            ext.insert(s(e, "nid").to_string(), s(e, "label").to_string());
        }
    }
    if ext.is_empty() {
        return;
    }
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
        .collect::<BTreeSet<_>>();
    let mut defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes.iter() {
        let id = s(n, "id");
        let l = s(n, "label");
        if !ext.contains_key(id)
            && !l.is_empty()
            && family(s(n, "source_file")) == Some("native")
            && !is_language_builtin(l)
            && !s(n, "source_file").is_empty()
            && contained.contains(id)
            && is_type_like_definition(n)
        {
            defs.entry(l.into()).or_default().push(id.into());
        }
    }
    let mut map = BTreeMap::new();
    for (id, label) in ext {
        if let Some(c) = defs.get(&label).filter(|c| c.len() == 1)
            && c[0] != id
        {
            map.insert(id, c[0].clone());
        }
    }
    if map.is_empty() {
        return;
    }
    nodes.retain(|n| !map.contains_key(s(n, "id")));
    let mut seen = BTreeSet::new();
    edges.retain_mut(|e| {
        let src = map.get(s(e, "source")).cloned();
        let tgt = map.get(s(e, "target")).cloned();
        let changed = src.is_some() || tgt.is_some();
        if let Some(s) = src {
            e["source"] = json!(s);
        }
        if let Some(t) = tgt {
            e["target"] = json!(t);
        }
        if changed && e["source"] == e["target"] {
            return false;
        }
        let key = [
            "source",
            "target",
            "relation",
            "source_file",
            "source_location",
        ]
        .map(|k| e.get(k).cloned().unwrap_or(Value::Null).to_string());
        let unique = seen.insert(key);
        !changed || unique
    });
}
/// `_merge_csharp_partial_class_nodes` (#2332, #2411): one node per C#
/// partial type. Runs after `disambiguate_colliding_node_ids`, so a half
/// whose id collides with another file's node is already salted apart
/// (0.9.91, #4411), and before the stub rewire and the resolvers.
pub fn merge_csharp_partials(
    per: &mut [Value],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    paths: &[PathBuf],
    root: &Path,
) {
    merge_csharp_partials_of(per, nodes, edges, paths, root);
}
/// [`merge_csharp_partials`] over per-file results in either form.
pub(crate) fn merge_csharp_partials_of<P: PerFile>(
    per: &mut [P],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    paths: &[PathBuf],
    root: &Path,
) {
    let mut id_path = BTreeMap::new();
    for (r, p) in per.iter().zip(paths) {
        for n in r.nodes().into_iter().flatten() {
            id_path.entry(rs(n, "id").to_string()).or_insert(p);
        }
    }
    let stop = resolved(root);
    let projects = paths
        .iter()
        .filter(|p| {
            [".csproj", ".fsproj", ".vbproj"]
                .contains(&crate::extract_languages::suffix(p).as_str())
        })
        .map(|p| resolved(p).parent().unwrap_or(Path::new(".")).to_path_buf())
        .collect::<BTreeSet<_>>();
    let assembly = |id: &str| {
        if let Some(p) = id_path.get(id) {
            for d in resolved(p).parent().into_iter().flat_map(Path::ancestors) {
                if projects.contains(d)
                    || std::fs::read_dir(d).is_ok_and(|items| {
                        items.flatten().any(|e| {
                            [".csproj", ".fsproj", ".vbproj"]
                                .contains(&crate::extract_languages::suffix(&e.path()).as_str())
                        })
                    })
                {
                    return d.to_string_lossy().into_owned();
                }
                if d == stop {
                    break;
                }
            }
        }
        String::new()
    };
    // Grouped by (namespace, type path, assembly). `type_path` names the type
    // with its enclosing types and arities (`Outer`1.Inner`), so nested halves
    // only meet under the same outer and `Foo` / `Foo<T>` stay apart (#4249,
    // #4363); a nested type without one stays unmerged.
    let mut groups: BTreeMap<(String, String, String), Vec<&Value>> = BTreeMap::new();
    for n in nodes.iter() {
        let md = &n["metadata"];
        if !is_cs_file(&n["source_file"])
            || s(n, "file_type") != "code"
            || !md.get("is_partial").is_some_and(truthy)
        {
            continue;
        }
        let type_path = match md.get("type_path").filter(|v| truthy(v)) {
            Some(path) => string(path),
            None if md.get("is_nested_type").is_some_and(truthy) => continue,
            None if truthy(&n["label"]) => string(&n["label"]),
            None => continue,
        };
        groups
            .entry((
                s(md, "namespace").to_string(),
                type_path,
                assembly(s(n, "id")),
            ))
            .or_default()
            .push(n);
    }
    let mut map = BTreeMap::new();
    for g in groups.values_mut().filter(|g| g.len() > 1) {
        g.sort_by_key(|n| (s(n, "source_file"), s(n, "source_location"), s(n, "id")));
        for n in &g[1..] {
            if n["id"] != g[0]["id"] {
                map.insert(s(n, "id").to_string(), s(g[0], "id").to_string());
            }
        }
    }
    if map.is_empty() {
        return;
    }
    nodes.retain(|n| !map.contains_key(s(n, "id")));
    // Only self-loops the merge itself creates are dropped, and only edges the
    // merge rewrote are deduped (Graphify 117e6b4): an untouched recursive call
    // anywhere in the corpus survives a partial class elsewhere.
    let key_of = |e: &Value| {
        [
            "source",
            "target",
            "relation",
            "source_file",
            "source_location",
        ]
        .map(|k| e.get(k).cloned().unwrap_or(Value::Null).to_string())
    };
    let mut seen = BTreeSet::new();
    let mut kept = Vec::with_capacity(edges.len());
    for mut e in std::mem::take(edges) {
        let src0 = s(&e, "source").to_string();
        let tgt0 = s(&e, "target").to_string();
        let src = map.get(&src0).cloned();
        let tgt = map.get(&tgt0).cloned();
        if src.is_none() && tgt.is_none() {
            seen.insert(key_of(&e));
            kept.push(e);
            continue;
        }
        let new_src = src.clone().unwrap_or_else(|| src0.clone());
        let new_tgt = tgt.clone().unwrap_or_else(|| tgt0.clone());
        if new_src == new_tgt && e.get("source") != e.get("target") {
            continue;
        }
        if let Some(id) = src {
            e["source"] = json!(id);
        }
        if let Some(id) = tgt {
            e["target"] = json!(id);
        }
        if seen.insert(key_of(&e)) {
            kept.push(e);
        }
    }
    *edges = kept;
    for r in per {
        if let Some(raw) = r.raw_calls_mut() {
            for call in raw {
                if let Some(id) = map.get(rs(call, "caller_nid")) {
                    call.rec_set_str("caller_nid", id);
                }
            }
        }
    }
}
/// `_drop_shadowed_csharp_partial_bases`: a merged partial type whose halves
/// resolved the same base differently (one half could not see it) keeps the
/// real base and drops the bare-name placeholder beside it. A placeholder
/// written qualified or through a `using` alias may be a different type that
/// only shares the simple name, so it stays (#4363).
pub fn drop_shadowed_csharp_partial_bases(nodes: &[Value], edges: &mut Vec<Value>) {
    // `str(item.get(key, ""))`.
    let text = |item: &Value, key: &str| {
        item.get(key)
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| string(v)))
            .unwrap_or_default()
    };
    let by_id = nodes
        .iter()
        .map(|n| (text(n, "id"), n))
        .collect::<FxHashMap<_, _>>();
    let node = |id: &str| by_id.get(id).copied().unwrap_or(&Value::Null);
    // `using X = ...;` aliases per file, from the C# import edges.
    let mut aliases: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for e in edges.iter() {
        let md = &e["metadata"];
        if !md.is_object() || md["using_kind"] != "alias" {
            continue;
        }
        if let Some(alias) = md["alias"].as_str().filter(|a| !a.is_empty()) {
            aliases
                .entry(text(node(&text(e, "source")), "source_file"))
                .or_default()
                .insert(alias.to_string());
        }
    }
    let partial_cs = |id: &str| {
        let n = node(id);
        text(n, "source_file").ends_with(".cs")
            && n["metadata"].get("is_partial").is_some_and(truthy)
    };
    let mut real: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for e in edges.iter() {
        if !matches!(s(e, "relation"), "inherits" | "implements") {
            continue;
        }
        let source = text(e, "source");
        if !partial_cs(&source) {
            continue;
        }
        let target = node(&text(e, "target"));
        if target.get("source_file").is_some_and(truthy) {
            real.entry(source)
                .or_default()
                .insert(text(target, "label"));
        }
    }
    if real.is_empty() {
        return;
    }
    edges.retain(|e| {
        if !matches!(s(e, "relation"), "inherits" | "implements") {
            return true;
        }
        let Some(names) = real.get(&text(e, "source")).filter(|n| !n.is_empty()) else {
            return true;
        };
        let target = node(&text(e, "target"));
        let label = text(target, "label");
        if target.get("source_file").is_some_and(truthy) || !names.contains(&label) {
            return true;
        }
        if e["metadata"].is_object() && e["metadata"].get("qualified").is_some_and(truthy) {
            return true;
        }
        aliases
            .get(&text(e, "source_file"))
            .is_some_and(|a| a.contains(&label))
    });
}
/// `_C_FAMILY_HEADER_EXTS`.
const C_FAMILY_HEADER_EXTS: &[&str] = &[".h", ".hpp", ".hh", ".hxx", ".cuh", ".inl"];
/// `_resolve_c_include_edges` (#4354): a quoted `#include`/`#import` the
/// including file's directory could not satisfy (its edge carries
/// `metadata.raw_include` and `include_kind: "quoted"`) binds to the header
/// file node whose path ends with the written path: exact case first, then
/// case-insensitively, and among several the one sharing the longest
/// directory prefix with the includer (a tie binds nothing). `./`, `../` and
/// absolute spellings are never guessed. A bound edge drops `target_file`.
pub fn resolve_c_include_edges(edges: &mut [Value], nodes: &[&Value], root: Option<&Path>) {
    if edges.is_empty() || nodes.is_empty() {
        return;
    }
    let key_of = |source: &str| match root {
        Some(root) => crate::extractors::resolution::source_key(source, root),
        None => source.to_string(),
    };
    let mut exact: FxHashMap<String, Vec<(&Value, String)>> = FxHashMap::default();
    let mut folded: FxHashMap<String, Vec<(&Value, String)>> = FxHashMap::default();
    for n in nodes.iter().copied() {
        if s(n, "type") == "namespace" || s(n, "file_type") == "rationale" {
            continue;
        }
        let Some(sf) = n.get("source_file").filter(|v| truthy(v)).map(string) else {
            continue;
        };
        let name = Path::new(&sf)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !C_FAMILY_HEADER_EXTS
            .contains(&crate::path_syntax::suffix(&name).to_lowercase().as_str())
            || n.get("label").and_then(Value::as_str) != Some(name.as_str())
            || !n.get("id").is_some_and(truthy)
        {
            continue;
        }
        let candidate = key_of(&sf).replace('\\', "/");
        let candidate = candidate.trim_end_matches('/').to_string();
        let basename = candidate.rsplit('/').next().unwrap_or("").to_string();
        exact
            .entry(basename.clone())
            .or_default()
            .push((n, candidate.clone()));
        folded
            .entry(basename.to_lowercase())
            .or_default()
            .push((n, candidate));
    }
    if exact.is_empty() {
        return;
    }
    for e in edges.iter_mut() {
        if s(e, "relation") != "imports" {
            continue;
        }
        let md = &e["metadata"];
        if !md.is_object() || md["include_kind"] != "quoted" {
            continue;
        }
        let Some(raw) = md["raw_include"].as_str().filter(|r| !r.is_empty()) else {
            continue;
        };
        let wanted = raw.replace('\\', "/");
        let wanted = wanted.trim_matches(crate::dedup_strings::whitespace);
        if wanted.is_empty()
            || wanted.starts_with("./")
            || wanted.starts_with("../")
            || wanted.starts_with('/')
            || Path::new(raw).is_absolute()
        {
            continue;
        }
        let base = wanted.rsplit('/').next().unwrap_or("");
        if base.is_empty() {
            continue;
        }
        let suffix = format!("/{wanted}");
        let mut matches = exact
            .get(base)
            .into_iter()
            .flatten()
            .filter(|(_, c)| c == wanted || c.ends_with(&suffix))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            let (lower, lower_suffix) = (wanted.to_lowercase(), suffix.to_lowercase());
            matches = folded
                .get(&base.to_lowercase())
                .into_iter()
                .flatten()
                .filter(|(_, c)| {
                    let c = c.to_lowercase();
                    c == lower || c.ends_with(&lower_suffix)
                })
                .collect();
        }
        let winner = match matches.as_slice() {
            [] => continue,
            [(node, _)] => Some(*node),
            _ => {
                let includer = key_of(s(e, "source_file")).replace('\\', "/");
                let includer = includer.trim_end_matches('/');
                let dirs = |p: &str| {
                    let mut parts = p.split('/').map(str::to_string).collect::<Vec<_>>();
                    parts.pop();
                    parts
                };
                let mine = dirs(includer);
                let scored = matches
                    .iter()
                    .map(|(node, c)| {
                        let shared = mine
                            .iter()
                            .zip(dirs(c).iter())
                            .take_while(|(a, b)| a == b)
                            .count();
                        (shared, *node)
                    })
                    .collect::<Vec<_>>();
                let best = scored.iter().map(|(score, _)| *score).max().unwrap_or(0);
                let top = scored
                    .iter()
                    .filter(|(score, _)| *score == best)
                    .collect::<Vec<_>>();
                (top.len() == 1).then(|| top[0].1)
            }
        };
        if let Some(winner) = winner {
            e["target"] = winner["id"].clone();
            if let Some(m) = e.as_object_mut() {
                m.shift_remove("target_file");
            }
        }
    }
}
pub fn repoint_python_imports(
    paths: &[PathBuf],
    nodes: &[Value],
    edges: &mut [Value],
    root: &Path,
) {
    let root = resolved(root);
    let ids = nodes.iter().map(|n| s(n, "id")).collect::<BTreeSet<_>>();
    let mut aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut sibling: BTreeMap<PathBuf, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    // `_repoint_python_package_imports` and `_repoint_python_sibling_imports`
    // in one pass: Python by effective suffix, packages by any initializer the
    // project declares Python (#2961), and a loose module or package named
    // like a stdlib module never captures a bare import of it (#4261).
    let python = |p: &Path| [".py", ".pyi"].contains(&effective_suffix(p).to_lowercase().as_str());
    let init = |p: &Path| {
        p.file_stem().is_some_and(|s| s == "__init__")
            && [".py", ".pyi"].contains(&effective_suffix(p).as_str())
    };
    for p in paths {
        if !python(p) {
            continue;
        }
        let full = resolved(p);
        let Ok(rel) = full.strip_prefix(&root) else {
            continue;
        };
        let parts = rel.components().collect::<Vec<_>>();
        let mut dir = full.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut levels = 0;
        while levels < parts.len().saturating_sub(1) && is_python_package_dir(&dir, &[".py"]) {
            levels += 1;
            dir = dir.parent().unwrap_or(&dir).to_path_buf();
        }
        let file = file_node_id(rel);
        if levels > 0 && levels + 1 < parts.len() {
            let module = parts[parts.len() - levels - 1..]
                .iter()
                .collect::<PathBuf>();
            aliases
                .entry(file_node_id(&module))
                .or_default()
                .insert(file.clone());
            if init(p) {
                aliases
                    .entry(make_id([module
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_string_lossy()
                        .as_ref()]))
                    .or_default()
                    .insert(file.clone());
            }
        }
        if !ids.contains(file.as_str()) {
            continue;
        }
        let is_pkg = init(&full);
        let parent = full.parent().unwrap_or(Path::new("."));
        let d = if is_pkg {
            parent.parent().unwrap_or(parent)
        } else {
            parent
        };
        let module = if is_pkg {
            parent.file_name()
        } else {
            full.file_stem()
        }
        .unwrap_or_default()
        .to_string_lossy();
        if !is_python_stdlib_module(&module) && !is_python_package_dir(d, &[".py", ".pyi"]) {
            sibling
                .entry(d.to_path_buf())
                .or_default()
                .entry(make_id([module.as_ref()]))
                .or_default()
                .insert(file);
        }
    }
    for e in edges {
        if !["imports", "imports_from"].contains(&s(e, "relation"))
            || !python(Path::new(s(e, "source_file")))
        {
            continue;
        }
        let target = s(e, "target");
        if !ids.contains(target)
            && let Some(targets) = aliases.get(target).filter(|t| t.len() == 1)
        {
            e["target"] = json!(targets.iter().next().unwrap());
        }
        let sf = s(e, "source_file");
        if sf.is_empty() {
            continue;
        }
        let full = resolved(&if Path::new(sf).is_absolute() {
            PathBuf::from(sf)
        } else {
            root.join(sf)
        });
        if let Some(t) = full
            .parent()
            .and_then(|p| sibling.get(p))
            .and_then(|a| a.get(s(e, "target")))
            .filter(|t| t.len() == 1)
        {
            let target = t.iter().next().unwrap();
            if target != s(e, "source") {
                e["target"] = json!(target);
            }
        }
    }
}
