// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Pinned language configurations and native import callbacks.
use super::{
    base::{file_stem, make_id, read_text},
    engine_types::KindName,
    engine_types::{children, first_child, named},
    models::LanguageConfig,
    resolution_js as js,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use tree_sitter::Node;
pub static CONFIGS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../assets/engine/configs.json")).unwrap());
pub fn config(name: &str) -> crate::Result<LanguageConfig> {
    let key = if name.starts_with('_') {
        name.to_string()
    } else {
        format!("_{}_CONFIG", name.to_uppercase())
    };
    let mut c: LanguageConfig = serde_json::from_value(
        CONFIGS
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("Unknown language configuration: {name}"))?,
    )
    .map_err(crate::Error::from)?;
    let module = c.ts_module.clone();
    if CONFIGS[&key]["_callbacks"]["import_handler"].is_string() {
        c.import_handler = Some(Arc::new(move |n, s, f, _st, e, p, sc, root| {
            import(&module, n, s, f, e, p, sc, root)
        }));
    }
    match c.ts_module.as_str() {
        "tree_sitter_cpp" => {
            c.resolve_function_name_fn = Some(Arc::new(super::engine_scope::get_cpp_func_name))
        }
        "tree_sitter_c" => c.resolve_function_name_fn = Some(Arc::new(c_func_name)),
        "tree_sitter_ruby" => c.sanitize_symbol_name_fn = Some(Arc::new(ruby_sanitize)),
        _ => {}
    }
    Ok(c)
}
pub fn c_func_name(n: Node<'_>, s: &[u8]) -> Option<String> {
    if n.kind_name() == "identifier" {
        return Some(read_text(n, s));
    }
    if let Some(d) = n.child_by_field_name("declarator") {
        return c_func_name(d, s);
    }
    first_child(n, &["identifier"]).map(|n| read_text(n, s))
}
pub fn ruby_sanitize(name: &str) -> String {
    for (suffix, extra) in [('!', "_bang"), ('?', "_pred"), ('=', "_eq")] {
        if let Some(s) = name.strip_suffix(suffix) {
            return format!("{s}{extra}");
        }
    }
    name.into()
}
/// `_probe_python_module_candidate`: a package (`__init__.py`), the exact
/// file, or the module with `.py` appended, each still Python under the
/// project's language overrides; native suffixes before declared ones.
pub fn probe_python_module_candidate(p: &Path) -> Option<PathBuf> {
    use super::resolution_paths::probe_declared_python_module_candidate as declared;
    use crate::rcfile::effective_suffix;
    if p.is_dir() {
        let init = p.join("__init__.py");
        if init.is_file() && effective_suffix(&init) == ".py" {
            return Some(init);
        }
    }
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    if p.file_name().is_none() {
        return declared(p);
    }
    let q = p.with_extension("py");
    if q.is_file() && effective_suffix(&q) == ".py" {
        return Some(q);
    }
    declared(p)
}
#[allow(clippy::too_many_arguments)]
pub fn import(
    module: &str,
    n: Node<'_>,
    s: &[u8],
    file: &str,
    edges: &mut Vec<Value>,
    path: &str,
    scope: Option<&[String]>,
    scan_root: Option<&Path>,
) -> Option<Vec<(String, String)>> {
    let line = n.start_position().row + 1;
    let edge = |target: String, rel: &str, context: &str| json!({"source":file,"target":target,"relation":rel,"context":context,"confidence":"EXTRACTED","source_file":path,"source_location":format!("L{line}"),"weight":1.0});
    match module {
        "tree_sitter_python" => python_import(n, s, file, edges, path, scan_root),
        "tree_sitter_javascript" | "tree_sitter_typescript" => {
            let reexport = n.kind_name() == "export_statement";
            // Only a re-export (`export ... from '...'`) names a source module;
            // `export default "x"` has a string but no `from` (0.9.85, #4166).
            if reexport
                && !children(n).iter().any(|c| {
                    c.kind_name() == "from"
                        || (c.kind_name() == "identifier" && read_text(*c, s) == "from")
                })
            {
                return None;
            }
            let type_only = children(n)
                .iter()
                .any(|c| c.kind_name() == "type" && !c.is_named());
            let module = first_child(n, &["string"]).or_else(|| {
                first_child(n, &["import_require_clause"]).and_then(|c| first_child(c, &["string"]))
            });
            if let Some(m) = module {
                let raw = read_text(m, s);
                let raw = raw.trim_matches(['\'', '"', '`', ' ']);
                if let Some((mut id, mut target)) = js::resolve_js_import_target(raw, path) {
                    if target.as_ref().is_some_and(|p| !p.is_file()) {
                        id = make_id(["ref", raw]);
                        target = None;
                    }
                    let mut e = edge(
                        id,
                        "imports_from",
                        if reexport { "re-export" } else { "import" },
                    );
                    if let Some(p) = &target {
                        e["target_file"] = json!(p);
                    }
                    if type_only {
                        e["type_only"] = json!(true);
                    }
                    edges.push(e);
                    if let Some(p) = target {
                        let stem = file_stem(&p);
                        let mut stack = children(n);
                        while let Some(c) = stack.pop() {
                            let desired = if reexport {
                                "export_specifier"
                            } else {
                                "import_specifier"
                            };
                            if c.kind_name() == desired {
                                if let Some(name) = c.child_by_field_name("name") {
                                    let name = read_text(name, s);
                                    if reexport && name == "default" {
                                        continue;
                                    }
                                    let mut e = edge(
                                        make_id([stem.as_str(), name.as_str()]),
                                        if reexport { "re_exports" } else { "imports" },
                                        if reexport { "re-export" } else { "import" },
                                    );
                                    e["target_file"] = json!(p);
                                    if type_only && reexport {
                                        e["type_only"] = json!(true);
                                    }
                                    edges.push(e);
                                }
                            } else {
                                stack.extend(children(c).into_iter().rev());
                            }
                        }
                    }
                }
            }
        }
        "tree_sitter_c" | "tree_sitter_cpp" => {
            if let Some(c) = first_child(n, &["string_literal", "system_lib_string", "string"]) {
                let raw = read_text(c, s);
                let raw = raw.trim_matches(['"', '<', '>', ' ']);
                if c.kind_name() != "system_lib_string"
                    && let Ok(Some(p)) = super::resolution_paths::resolve_c_include_path(raw, path)
                {
                    let mut e = edge(make_id([p.to_string_lossy().as_ref()]), "imports", "import");
                    e["target_file"] = json!(p);
                    edges.push(e);
                    return None;
                }
                let name = raw
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("");
                if !name.is_empty() {
                    let mut e = edge(make_id([name]), "imports", "import");
                    // A quoted include that does not resolve beside the file
                    // keeps its spelling for the corpus include pass (#4354).
                    if c.kind_name() != "system_lib_string" {
                        let m = json!({"raw_include": raw, "include_kind": "quoted"});
                        e["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&m)));
                    }
                    edges.push(e);
                }
            }
        }
        "tree_sitter_c_sharp" => {
            let text = read_text(n, s);
            let text = text
                .trim()
                .trim_end_matches(';')
                .trim_start_matches("global ")
                .trim();
            let body = text.strip_prefix("using").map(str::trim)?;
            let (kind, alias, target) = if let Some(t) = body.strip_prefix("static ") {
                ("static", None, t.trim())
            } else if let Some((a, t)) = body.split_once('=') {
                ("alias", Some(a.trim()), t.trim())
            } else {
                ("namespace", None, body)
            };
            if !target.is_empty() {
                let sc = scope.unwrap_or_default();
                // Upstream's key order: using_kind, alias, target_fqn,
                // scope_kind, scope_id (None values dropped).
                let mut m = json!({"using_kind":kind});
                if let Some(a) = alias {
                    m["alias"] = json!(a);
                }
                m["target_fqn"] = json!(target);
                m["scope_kind"] = json!(if sc.is_empty() { "file" } else { "namespace" });
                if let Some(id) = sc.last() {
                    m["scope_id"] = json!(id);
                }
                let mut e = edge(make_id([target]), "imports", "import");
                e["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&m)));
                edges.push(e);
            }
        }
        "tree_sitter_java" | "tree_sitter_groovy" => {
            if let Some(c) = first_child(n, &["scoped_identifier", "identifier"]) {
                let raw = read_text(c, s);
                let segs: Vec<_> = raw.split('.').collect();
                let mut name = segs.last().copied().unwrap_or("").trim_matches(['*', '.']);
                if name.is_empty() && segs.len() > 1 {
                    name = segs[segs.len() - 2];
                }
                if !name.is_empty() {
                    edges.push(edge(make_id([name]), "imports", "import"));
                }
            }
        }
        "tree_sitter_kotlin" => {
            if let Some(c) = n
                .child_by_field_name("path")
                .or_else(|| first_child(n, &["qualified_identifier", "identifier"]))
            {
                let raw = read_text(c, s);
                let raw = raw.trim();
                if raw.is_empty()
                    || raw.ends_with(".*")
                    || children(n).iter().any(|c| c.kind_name() == "*")
                {
                    return None;
                }
                let mut alias = None;
                let mut saw = false;
                for c in children(n) {
                    if saw && matches!(c.kind_name(), "identifier" | "simple_identifier") {
                        alias = Some(read_text(c, s));
                        break;
                    }
                    saw |= c.kind_name() == "as";
                }
                let name = raw.rsplit('.').next().unwrap_or("").trim();
                if !name.is_empty() {
                    let mut e = edge(make_id([name]), "imports", "import");
                    let mut m = json!({"target_fqn":raw});
                    if let Some(a) = alias {
                        m["alias"] = json!(a.trim());
                    }
                    e["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&m)));
                    edges.push(e);
                }
            }
        }
        "tree_sitter_scala" => {
            if let Some(c) = first_child(n, &["stable_id", "identifier"]) {
                let raw = read_text(c, s);
                let name = raw
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim_matches(['{', '}', ' ']);
                if !name.is_empty() && name != "_" {
                    edges.push(edge(make_id([name]), "imports", "import"));
                }
            }
        }
        "tree_sitter_php" => {
            let (mut name, mut saw) = (None, false);
            for c in children(n) {
                if c.kind_name() == "as" {
                    saw = true;
                    continue;
                }
                if matches!(c.kind_name(), "qualified_name" | "name" | "identifier") {
                    let raw = read_text(c, s);
                    let bare = raw.rsplit('\\').next().unwrap_or("").trim().to_string();
                    if saw {
                        name = Some(bare);
                        break;
                    }
                    if name.is_none() {
                        name = Some(bare);
                    }
                }
            }
            if let Some(name) = name.filter(|s| !s.is_empty()) {
                edges.push(edge(make_id([name.as_str()]), "imports", "import"));
            }
        }
        "tree_sitter_swift" => {
            if let Some(c) = first_child(n, &["identifier"]) {
                let raw = read_text(c, s);
                let id = make_id([raw.as_str()]);
                edges.push(edge(id.clone(), "imports", "import"));
                return Some(vec![(id, raw)]);
            }
        }
        "tree_sitter_lua" => {
            let re = regex::Regex::new(r#"require\s*(?:\(\s*)?["']([^"']+)["']"#).unwrap();
            for cap in re.captures_iter(&read_text(n, s)) {
                let target = js::resolve_lua_import_target(&cap[1], path);
                let mut e = edge(target, "imports", "import");
                e["source_location"] = json!(line.to_string());
                e["confidence_score"] = json!(1.0);
                edges.push(e);
            }
        }
        _ => {}
    }
    None
}
/// `extract._import_python`: absolute imports resolve through the shared
/// scan-root-aware module resolver (#3729), so an edge targets the imported
/// file's own node. Edges carry transient `_python_import_*` keys that the
/// corpus-wide ambiguity guard consumes and strips before resolution.
fn python_import(
    n: Node<'_>,
    s: &[u8],
    file: &str,
    edges: &mut Vec<Value>,
    path: &str,
    scan_root: Option<&Path>,
) {
    use super::resolution::resolved;
    use super::resolution_facts::{
        python_import_bindings, resolve_python_module_path, resolve_python_namespace_dir,
    };
    let line = n.start_position().row + 1;
    let edge = |target: &str, rel: &str| json!({"source":file,"target":target,"relation":rel,"context":"import","confidence":"EXTRACTED","source_file":path,"source_location":format!("L{line}"),"weight":1.0});
    let current = resolved(Path::new(path));
    let root = resolved(
        &scan_root
            .map(Path::to_path_buf)
            .or_else(|| current.parent().map(Path::to_path_buf))
            .unwrap_or_default(),
    );
    // An import of the importing file itself never becomes a self-loop.
    let not_self = |p: PathBuf| (resolved(&p) != current).then_some(p);
    let pairs = |pairs: Vec<(String, String)>| {
        pairs
            .into_iter()
            .map(|(a, b)| json!([a, b]))
            .collect::<Vec<_>>()
    };
    // Imports under `if TYPE_CHECKING:` never run, so they are stamped
    // `type_only` like TS `import type`: cycle detection skips them, the edge
    // stays (0.9.80, #3159).
    let type_only = super::engine_scope::python_under_type_checking(n, s);
    if n.kind_name() == "import_statement" {
        for child in children(n)
            .into_iter()
            .filter(|c| matches!(c.kind_name(), "dotted_name" | "aliased_import"))
        {
            let raw = read_text(child, s);
            let (raw_module, raw_alias) = raw.split_once(" as ").unwrap_or((&raw, ""));
            let module = raw_module.trim().trim_start_matches('.');
            let binding = if raw_alias.is_empty() {
                module.split('.').next().unwrap_or("")
            } else {
                raw_alias.trim()
            };
            let target = resolve_python_module_path(module, &current, &root, 0).and_then(not_self);
            let id = target.as_ref().map_or_else(
                || make_id([module]),
                |p| make_id([p.to_string_lossy().as_ref()]),
            );
            let mut e = edge(&id, "imports");
            e["_python_import_module"] = json!(module);
            e["_python_import_bindings"] = json!(pairs(vec![(module.into(), binding.into())]));
            e["_python_import_module_binding"] = json!(true);
            if let Some(p) = &target {
                e["target_file"] = json!(p);
            }
            if !raw_alias.is_empty() {
                e["local_alias"] = json!(raw_alias.trim());
            }
            if type_only {
                e["type_only"] = json!(true);
            }
            edges.push(e);
        }
    } else if n.kind_name() == "import_from_statement"
        && let Some(m) = n.child_by_field_name("module_name")
    {
        let raw = read_text(m, s);
        let relative = raw.starts_with('.');
        let (id, target) = if relative {
            let dots = raw.chars().take_while(|c| *c == '.').count();
            let name = raw.trim_start_matches('.');
            match resolve_python_module_path(name, &current, &root, dots) {
                Some(target) => (make_id([target.to_string_lossy().as_ref()]), Some(target)),
                None => {
                    let mut base = current.parent().unwrap_or(Path::new("")).to_path_buf();
                    for _ in 1..dots {
                        base = base.parent().map(Path::to_path_buf).unwrap_or(base);
                    }
                    let target = base.join(if name.is_empty() {
                        "__init__.py".into()
                    } else {
                        format!("{}.py", name.replace('.', "/"))
                    });
                    // No file behind the import: an id from the attempted
                    // absolute path would bake the checkout location into the
                    // graph, so it is the module name relative to the scan
                    // root, as an unresolved absolute import gets (0.9.78, #4157).
                    let id = if target.is_file() {
                        make_id([target.to_string_lossy().as_ref()])
                    } else {
                        make_id([missing_relative_module_name(&target, &root, &raw).as_str()])
                    };
                    (id, Some(target))
                }
            }
        } else {
            let target = resolve_python_module_path(&raw, &current, &root, 0);
            if target.is_none() && resolve_python_namespace_dir(&raw, &current, &root, 0).is_some()
            {
                // A namespace package has no file node, but the names it
                // imports can still bind to ambiguous submodules: a private
                // marker lets the corpus guard suppress those bindings.
                edges.push(json!({"source":file,"target":"","relation":"_python_import_marker","source_file":path,"_python_import_module":raw,"_python_import_bindings":pairs(python_import_bindings(n, s)),"_python_import_marker_only":true}));
                return;
            }
            let target = target.and_then(not_self);
            let id = target.as_ref().map_or_else(
                || make_id([raw.as_str()]),
                |p| make_id([p.to_string_lossy().as_ref()]),
            );
            (id, target)
        };
        let mut e = edge(&id, "imports_from");
        if !relative {
            e["_python_import_module"] = json!(raw);
            e["_python_import_bindings"] = json!(pairs(python_import_bindings(n, s)));
        }
        if let Some(p) = target.filter(|p| p.is_file()) {
            e["target_file"] = json!(p);
        }
        if type_only {
            e["type_only"] = json!(true);
        }
        edges.push(e);
    }
}
/// `_missing_relative_module_name` (0.9.78, #4157): the dotted module name,
/// relative to `root`, of an unresolved relative import's attempted file
/// (`pkg/missing.py` or `pkg/__init__.py`); `ref:<raw>` when the import
/// climbs above the scan root, so the id never carries an absolute path.
fn missing_relative_module_name(attempted: &Path, root: &Path, raw: &str) -> String {
    let Ok(rel) = attempted.strip_prefix(root) else {
        return format!("ref:{raw}");
    };
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = parts.last_mut() {
        // `with_suffix("")`: drop the final suffix, as PurePath does.
        let cut = last.len() - crate::path_syntax::suffix(last).len();
        last.truncate(cut);
    }
    if parts.last().is_some_and(|p| p == "__init__") {
        parts.pop();
    }
    if parts.is_empty() {
        format!("ref:{raw}")
    } else {
        parts.join(".")
    }
}
pub fn descendant<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if n.kind_name() == kind {
        return Some(n);
    }
    for c in named(n) {
        if let Some(found) = descendant(c, kind) {
            return Some(found);
        }
    }
    None
}
