//! Conservative bindings within an authorized source manifest. No files outside
//! that manifest are consulted, and unresolved receivers remain unresolved.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn items<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn visible(scope: &str, caller: &str) -> bool {
    scope.is_empty()
        || caller == scope
        || caller
            .strip_prefix(scope)
            .is_some_and(|r| r.starts_with('.'))
}

/// Strip JSONC comments/trailing commas without touching quoted content.
pub(crate) fn configuration(content: &str) -> Option<Value> {
    let chars: Vec<_> = content.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut quoted = false;
    while i < chars.len() {
        let c = chars[i];
        if quoted {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                i += 1;
                out.push(chars[i]);
            } else if c == '"' {
                quoted = false;
            }
        } else if c == '"' {
            quoted = true;
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push('\n');
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 == chars.len() {
                return None;
            }
            i += 1;
            out.push(' ');
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
        } else {
            out.push(c);
        }
        i += 1;
    }
    if quoted {
        return None;
    }
    // A second pass also handles a comment between a trailing comma and `}`.
    if let Ok(value) = serde_json::from_str::<Value>(&out) {
        return Some(value);
    }
    if out != content {
        configuration(&out)
    } else {
        None
    }
}

fn join(parent: &str, child: &str) -> Option<String> {
    crate::structure::normalize(&Path::new(parent).join(child).to_string_lossy())
}
fn parent(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("")
}

pub(crate) fn javascript_targets(base: &str, paths: &BTreeSet<String>) -> BTreeSet<String> {
    if paths.contains(base) {
        return BTreeSet::from([base.to_string()]);
    }
    let mut roots = vec![base.to_string()];
    for suffix in [".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(root) = base.strip_suffix(suffix) {
            roots.push(root.to_string());
        }
    }
    let mut targets = BTreeSet::new();
    for root in roots {
        for suffix in ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
            for path in [format!("{root}.{suffix}"), format!("{root}/index.{suffix}")] {
                if paths.contains(&path) {
                    targets.insert(path);
                }
            }
        }
    }
    targets
}

#[derive(Default)]
struct Paths {
    base: Option<String>,
    entries: BTreeMap<String, Vec<String>>,
}
fn load_paths(
    path: &str,
    analyses: &serde_json::Map<String, Value>,
    visiting: &mut BTreeSet<String>,
) -> Option<Paths> {
    if visiting.len() >= 8 || !visiting.insert(path.to_string()) {
        return None;
    }
    let config = analyses.get(path)?.get("module_configuration")?;
    let mut result = Paths::default();
    if let Some(extends) = config["extends"].as_str() {
        // Package-based tsconfig inheritance requires a package resolver.
        if !extends.starts_with('.') {
            return None;
        }
        let mut base = join(parent(path), extends)?;
        if !base.ends_with(".json") {
            base.push_str(".json");
        }
        result = load_paths(&base, analyses, visiting)?;
    }
    if let Some(base) = config["compilerOptions"]["baseUrl"].as_str() {
        result.base = Some(join(parent(path), base)?);
    }
    if let Some(entries) = config["compilerOptions"]["paths"].as_object() {
        if entries.len() > 128 {
            return None;
        }
        result.entries.clear();
        for (pattern, values) in entries {
            if pattern.matches('*').count() > 1 {
                continue;
            }
            let values: Vec<_> = values
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(16)
                .filter_map(|value| join(result.base.as_deref().unwrap_or(parent(path)), value))
                .collect();
            result.entries.insert(pattern.clone(), values);
        }
    }
    visiting.remove(path);
    Some(result)
}

pub(crate) fn configured_targets(
    source: &str,
    module: &str,
    analyses: &serde_json::Map<String, Value>,
    paths: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut configs: Vec<_> = paths
        .iter()
        .filter(|path| {
            matches!(
                Path::new(path).file_name().and_then(|s| s.to_str()),
                Some("tsconfig.json" | "jsconfig.json")
            ) && (parent(path).is_empty() || source.starts_with(&format!("{}/", parent(path))))
        })
        .collect();
    configs.sort_by_key(|path| std::cmp::Reverse(parent(path).len()));
    let Some(config) = configs
        .first()
        .and_then(|p| load_paths(p, analyses, &mut BTreeSet::new()))
    else {
        return BTreeSet::new();
    };
    let mut matches = Vec::new();
    for (pattern, bases) in &config.entries {
        let matched = if let Some((prefix, suffix)) = pattern.split_once('*') {
            module
                .strip_prefix(prefix)
                .and_then(|s| s.strip_suffix(suffix))
                .map(|middle| (middle, prefix.len(), suffix.len()))
        } else if pattern == module {
            Some(("", usize::MAX, usize::MAX))
        } else {
            None
        };
        if let Some((middle, prefix, suffix)) = matched {
            matches.push(((prefix, suffix), middle, bases));
        }
    }
    matches.sort_by_key(|v| std::cmp::Reverse(v.0));
    if let Some((_, middle, bases)) = matches.first() {
        // Configured fallback targets are ordered, just as in TypeScript.
        for base in *bases {
            let targets = javascript_targets(&base.replace('*', middle), paths);
            if !targets.is_empty() {
                return targets;
            }
        }
        return BTreeSet::new();
    }
    config
        .base
        .and_then(|base| join(&base, module))
        .map(|base| javascript_targets(&base, paths))
        .unwrap_or_default()
}

fn has_js_config(source: &str, paths: &BTreeSet<String>) -> bool {
    paths.iter().any(|path| {
        matches!(
            Path::new(path).file_name().and_then(|s| s.to_str()),
            Some("tsconfig.json" | "jsconfig.json")
        ) && (parent(path).is_empty() || source.starts_with(&format!("{}/", parent(path))))
    })
}

/// Nearest ancestor directory of `source` (walking up to the manifest root)
/// holding a `package.json` or `pnpm-workspace.yaml`, mirroring Graphify's
/// `_find_js_project_anchor`. Falls back to the importer's own directory.
fn js_project_anchor(source: &str, paths: &BTreeSet<String>) -> String {
    let start = parent(source).to_string();
    let mut current = start.clone();
    loop {
        for marker in ["package.json", "pnpm-workspace.yaml"] {
            let candidate = if current.is_empty() {
                marker.to_string()
            } else {
                format!("{current}/{marker}")
            };
            if paths.contains(&candidate) {
                return current;
            }
        }
        if current.is_empty() {
            break;
        }
        current = parent(&current).to_string();
    }
    start
}

/// Unmapped `@/` project-root convention fallback (Graphify #3357). Active only
/// when no `tsconfig.json`/`jsconfig.json` governs `source`: `@/x` resolves to
/// `<anchor>/src/x` when the anchor has a `src/` directory, else `<anchor>/x`.
pub(crate) fn convention_alias_targets(
    source: &str,
    module: &str,
    paths: &BTreeSet<String>,
) -> BTreeSet<String> {
    let Some(subpath) = module.strip_prefix("@/") else {
        return BTreeSet::new();
    };
    if subpath.is_empty() || has_js_config(source, paths) {
        return BTreeSet::new();
    }
    let anchor = js_project_anchor(source, paths);
    let under = |dir: &str| {
        if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        }
    };
    let src = format!("{}src", under(&anchor));
    if paths
        .iter()
        .any(|path| path.starts_with(&format!("{src}/")))
        && let Some(base) = join(&src, subpath)
    {
        let targets = javascript_targets(&base, paths);
        if !targets.is_empty() {
            return targets;
        }
    }
    join(&anchor, subpath)
        .map(|base| javascript_targets(&base, paths))
        .unwrap_or_default()
}

fn declaration(analysis: &Value, name: &str, caller: &str) -> Option<String> {
    let mut matches: Vec<_> = items(analysis, "symbols")
        .iter()
        .filter(|v| {
            let full = s(v, "name");
            if full == name {
                return true;
            }
            let (scope, local) = full.rsplit_once('.').unwrap_or(("", full));
            local == name && visible(scope, caller)
        })
        .collect();
    matches.sort_by_key(|v| std::cmp::Reverse(s(v, "name").matches('.').count()));
    let first = matches.first()?;
    if matches.get(1).is_some_and(|second| {
        s(second, "name").matches('.').count() == s(first, "name").matches('.').count()
    }) {
        return None;
    }
    Some(s(first, "name").to_string())
}

/// `from pkg import name` binds the submodule `pkg.name` only when the package
/// `__init__` leaves `name` unbound (importlib's `_handle_fromlist` imports the
/// submodule only for a missing attribute). That needs a complete `__init__`
/// analysis with no other top-level binding of `name`; `from . import name`
/// binds that same submodule.
pub(crate) fn leaves_submodule(init: &Value, name: &str) -> bool {
    if init["complete"] != true
        || declaration(init, name, "").is_some()
        || items(init, "shadowed_bindings")
            .iter()
            .any(|v| s(v, "scope").is_empty() && s(v, "name") == name)
    {
        return false;
    }
    items(init, "imports")
        .iter()
        .filter(|import| s(import, "scope").is_empty())
        .all(|import| {
            if items(import, "names").is_empty() {
                // `import a.b` binds `a`; `import a.b as c` binds `c`.
                let alias = s(import, "alias");
                let bound = if alias.is_empty() {
                    s(import, "module").split('.').next().unwrap_or("")
                } else {
                    alias
                };
                return bound != name;
            }
            match import["bindings"].get(name).and_then(Value::as_str) {
                None => true,
                Some(original) => {
                    import["level"] == 1 && s(import, "module").is_empty() && original == name
                }
            }
        })
}

/// Follow explicit named exports only. An incomplete/cyclic traversal cannot be
/// mistaken for a unique target, even if a different branch found a candidate.
fn exported(
    path: &str,
    name: &str,
    analyses: &serde_json::Map<String, Value>,
    edges: &[Value],
    visiting: &mut BTreeSet<(String, String)>,
    remaining: &mut usize,
) -> Option<BTreeSet<(String, String)>> {
    let key = (path.to_string(), name.to_string());
    if *remaining == 0 || visiting.len() >= 16 || !visiting.insert(key.clone()) {
        return None;
    }
    *remaining -= 1;
    let analysis = analyses.get(path)?;
    if items(analysis, "shadowed_bindings")
        .iter()
        .any(|v| s(v, "scope").is_empty() && s(v, "name") == name)
    {
        return None;
    }
    let mut found = BTreeSet::new();
    if let Some(local) = declaration(analysis, name, "") {
        found.insert((path.to_string(), local));
    }
    for import in items(analysis, "imports") {
        if !s(import, "scope").is_empty()
            || import["type_only"] == true
            || (!path.ends_with(".py") && import["reexport"] != true)
        {
            continue;
        }
        let Some(original) = import["bindings"].get(name).and_then(Value::as_str) else {
            continue;
        };
        // A submodule edge binds a module object, never a symbol named `name`.
        for edge in edges.iter().filter(|v| {
            s(v, "source") == path
                && v["line"] == import["line"]
                && s(v, "module") == s(import, "module")
                && v["kind"] == "imports"
                && s(v, "submodule").is_empty()
        }) {
            found.extend(exported(
                s(edge, "target"),
                original,
                analyses,
                edges,
                visiting,
                remaining,
            )?);
        }
    }
    visiting.remove(&key);
    Some(found)
}

/// Resolve explicit imported callable names. Unknown object receiver types,
/// wildcard imports and shadowed identifiers never become guessed call edges.
pub(crate) fn resolve_calls(
    analyses: &serde_json::Map<String, Value>,
    edges: &[Value],
) -> (Vec<Value>, Vec<Value>) {
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for (path, analysis) in analyses {
        for call in items(analysis, "calls") {
            let name = s(call, "callee");
            let caller = s(call, "caller");
            let head = name.split(['.', ':']).next().unwrap_or(name);
            if items(analysis, "shadowed_bindings")
                .iter()
                .any(|v| s(v, "name") == head && visible(s(v, "scope"), caller))
            {
                unresolved.push(json!({"path":path,"caller":caller,"callee":name,"line":call["line"],"reason":"shadowed_or_dynamic_binding"}));
                continue;
            }
            let mut candidates = BTreeSet::new();
            let mut incomplete_export = false;
            if let Some(local) = declaration(analysis, name, caller) {
                candidates.insert((path.clone(), local));
            }
            if candidates.is_empty() {
                for import in items(analysis, "imports") {
                    if !visible(s(import, "scope"), caller)
                        || import["reexport"] == true
                        || import["type_only"] == true
                    {
                        continue;
                    }
                    let targets: Vec<_> = edges
                        .iter()
                        .filter(|edge| {
                            s(edge, "source") == path
                                && edge["line"] == import["line"]
                                && s(edge, "module") == s(import, "module")
                                && edge["kind"] == "imports"
                        })
                        .map(|edge| (s(edge, "target"), s(edge, "submodule")))
                        .collect();
                    for (target, submodule) in targets {
                        let imported = if !submodule.is_empty() {
                            // `from pkg import sub` binds the submodule object, so
                            // `sub.fn()` (or an alias's) names `fn` inside it.
                            import["bindings"].as_object().and_then(|bindings| {
                                bindings.iter().find_map(|(local, original)| {
                                    (original.as_str() == Some(submodule))
                                        .then(|| name.strip_prefix(&format!("{local}.")))
                                        .flatten()
                                        .map(str::to_string)
                                })
                            })
                        } else {
                            import["bindings"]
                                .get(name)
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| {
                                    let namespace = if !s(import, "namespace").is_empty() {
                                        s(import, "namespace")
                                    } else if path.ends_with(".py")
                                        && items(import, "names").is_empty()
                                    {
                                        if !s(import, "alias").is_empty() {
                                            s(import, "alias")
                                        } else {
                                            s(import, "module")
                                        }
                                    } else {
                                        ""
                                    };
                                    if namespace.is_empty() {
                                        return None;
                                    }
                                    name.strip_prefix(&format!("{namespace}."))
                                        .or_else(|| name.strip_prefix(&format!("{namespace}::")))
                                        .map(str::to_string)
                                })
                        };
                        if let Some(imported) = imported {
                            if imported.contains('.') || imported.contains("::") {
                                continue;
                            }
                            match exported(
                                target,
                                &imported,
                                analyses,
                                edges,
                                &mut BTreeSet::new(),
                                &mut 256,
                            ) {
                                Some(found) => candidates.extend(found),
                                None => incomplete_export = true,
                            }
                        }
                    }
                }
            }
            if candidates.len() == 1 && !incomplete_export {
                let (target, callee) = candidates.into_iter().next().unwrap();
                resolved.push(json!({"path":path,"caller":caller,"callee_path":target,"callee":callee,"line":call["line"],
                    "basis":if target==*path{"unique_visible_declaration_candidate"}else{"explicit_import_binding"},"runtime_verified":false}));
            } else {
                unresolved.push(json!({"path":path,"caller":caller,"callee":name,"line":call["line"],"reason":if incomplete_export {"incomplete_or_cyclic_export"} else if candidates.is_empty(){"external_or_dynamic_call"}else{"ambiguous_call_binding"}}));
            }
        }
    }
    (resolved, unresolved)
}
