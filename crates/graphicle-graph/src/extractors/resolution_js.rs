// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/resolution.py
//! JS/TS module, workspace, package exports and JSONC resolution.
use super::{base::make_id, resolution_paths::resolve_cached};
use crate::{Result, fsutil};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

pub(crate) type SourceCorpus = Arc<BTreeSet<PathBuf>>;
thread_local! {
    static SOURCE_CORPUS: RefCell<Option<SourceCorpus>> = const { RefCell::new(None) };
}
pub(crate) struct CorpusGuard(Option<SourceCorpus>);
impl Drop for CorpusGuard {
    fn drop(&mut self) {
        SOURCE_CORPUS.with(|scope| scope.replace(self.0.take()));
    }
}
pub(crate) fn source_corpus(paths: &[PathBuf]) -> SourceCorpus {
    Arc::new(paths.iter().map(|p| absolute(p)).collect())
}
pub(crate) fn enter_source_corpus(corpus: Option<SourceCorpus>) -> CorpusGuard {
    CorpusGuard(SOURCE_CORPUS.with(|scope| scope.replace(corpus)))
}
/// The corpus this thread resolves against, for handing to worker threads.
pub(crate) fn current_source_corpus() -> Option<SourceCorpus> {
    SOURCE_CORPUS.with(|scope| scope.borrow().clone())
}
fn source_file(path: &Path) -> bool {
    path.is_file()
        && SOURCE_CORPUS.with(|scope| {
            scope
                .borrow()
                .as_ref()
                .is_none_or(|files| files.contains(&absolute(path)))
        })
}

pub type Aliases = BTreeMap<String, Vec<String>>;
static ALIASES: LazyLock<Mutex<BTreeMap<PathBuf, Aliases>>> = LazyLock::new(Default::default);
static BASES: LazyLock<Mutex<BTreeMap<PathBuf, Option<PathBuf>>>> = LazyLock::new(Default::default);
type PackageImports = BTreeMap<PathBuf, Option<(PathBuf, Value)>>;
static IMPORTS: LazyLock<Mutex<PackageImports>> = LazyLock::new(Default::default);
pub fn clear() {
    ALIASES.lock().unwrap().clear();
    BASES.lock().unwrap().clear();
    IMPORTS.lock().unwrap().clear();
    super::models::WORKSPACE_PACKAGE_CACHE
        .lock()
        .unwrap()
        .clear();
    super::resolution_paths::clear_paths();
}
fn absolute(p: &Path) -> PathBuf {
    resolve_cached(p).unwrap_or_else(|_| p.to_path_buf())
}
pub fn norm(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out.file_name().is_some_and(|s| s != "..") {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            _ => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}
/// `_JS_RESOLVE_EXTS`: suffixes appended to an import specifier, in order.
pub const JS_RESOLVE_EXTS: &[&str] = &[
    ".ts", ".tsx", ".mts", ".cts", ".svelte", ".ejs", ".js", ".jsx", ".mjs", ".cjs",
];
/// `_JS_INDEX_FILES`: directory index files, in order.
pub const JS_INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.svelte",
    "index.ejs",
    "index.js",
    "index.jsx",
    "index.mjs",
];
/// A candidate the JS/TS passes read: its effective suffix (a project
/// declaration included, #2961) is a JS-family one.
fn js_family(path: &Path) -> bool {
    super::models::JS_CACHE_BYPASS_SUFFIXES
        .contains(&crate::rcfile::effective_suffix(path).as_str())
}
/// `_resolve_js_import_path`: the exact file, the `.ts`/`.tsx` source of a
/// `.js` (`.ts`, then `.tsx`) or `.jsx` specifier, the specifier with a JS
/// suffix appended (native suffixes before project-declared ones), then a
/// directory index.
pub fn resolve_js_import_path(p: &Path) -> PathBuf {
    let p = norm(p);
    if source_file(&p) {
        return p;
    }
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    // `.js` may spell a `.ts` or a `.tsx` source (#3486), `.jsx` a `.tsx` one.
    let mapped: &[&str] = match ext {
        "js" => &["ts", "tsx"],
        "jsx" => &["tsx"],
        _ => &[],
    };
    for ext in mapped {
        let q = p.with_extension(ext);
        if source_file(&q) && js_family(&q) {
            return q;
        }
    }
    let declared = super::resolution_paths::declared_language_suffixes(JS_RESOLVE_EXTS);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    for ext in JS_RESOLVE_EXTS
        .iter()
        .copied()
        .chain(declared.iter().map(String::as_str))
    {
        let q = p.with_file_name(format!("{name}{ext}"));
        if source_file(&q) && js_family(&q) {
            return q;
        }
    }
    if p.is_dir() {
        for name in JS_INDEX_FILES {
            let q = p.join(name);
            if source_file(&q) && js_family(&q) {
                return q;
            }
        }
        for ext in &declared {
            let q = p.join(format!("index{ext}"));
            if source_file(&q) {
                return q;
            }
        }
    }
    p
}
pub fn strip_jsonc(text: &str) -> String {
    let re = regex::Regex::new(r#"(?s)"(?:\\.|[^"\\])*"|/\*.*?\*/|//[^\n]*"#).unwrap();
    let stripped = re.replace_all(text, |c: &regex::Captures<'_>| {
        if c[0].starts_with('"') {
            c[0].to_string()
        } else {
            String::new()
        }
    });
    regex::Regex::new(r",(\s*[}\]])")
        .unwrap()
        .replace_all(&stripped, "$1")
        .into_owned()
}
pub fn read_json_config(p: &Path) -> Option<Value> {
    let raw = fsutil::read_text_lossy(p).ok()?;
    serde_json::from_str::<Value>(&raw)
        .or_else(|_| serde_json::from_str(&strip_jsonc(&raw)))
        .ok()
        .filter(Value::is_object)
}
pub fn read_tsconfig_aliases(p: &Path, base: &Path, seen: &mut BTreeSet<PathBuf>) -> Aliases {
    if !seen.insert(p.to_path_buf()) {
        return Aliases::new();
    }
    let Some(data) = read_json_config(p) else {
        return Aliases::new();
    };
    let mut out = Aliases::new();
    let parents: Vec<&str> = match &data["extends"] {
        Value::String(s) => vec![s],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    for parent in parents {
        if parent.is_empty() || parent.starts_with('@') {
            continue;
        }
        let mut q = absolute(&base.join(parent));
        if q.extension().is_none() {
            q.set_extension("json");
        }
        if q.exists() {
            out.extend(read_tsconfig_aliases(&q, q.parent().unwrap_or(base), seen));
        }
    }
    // `references` (the `tsc -b` solution layout, #3745): a root config with
    // `"files": []` keeps every `paths` block in referenced project configs.
    // Each merges like another parent; this config's own `paths` still win,
    // and the shared `seen` set guards reference cycles too.
    for reference in data["references"].as_array().into_iter().flatten() {
        let Some(path) = reference["path"].as_str().filter(|p| !p.is_empty()) else {
            continue;
        };
        let mut q = absolute(&base.join(path));
        if q.is_dir() {
            q = q.join("tsconfig.json");
        } else if q.extension().is_none() {
            q.set_extension("json");
        }
        if q.exists() {
            out.extend(read_tsconfig_aliases(&q, q.parent().unwrap_or(base), seen));
        }
    }
    let paths_base = base.join(
        data["compilerOptions"]["baseUrl"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("."),
    );
    if let Some(paths) = data["compilerOptions"]["paths"].as_object() {
        for (alias, targets) in paths {
            if let Some(targets) = targets.as_array() {
                let targets: Vec<String> = targets
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        paths_base
                            .join(s)
                            .components()
                            .collect::<PathBuf>()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                if !targets.is_empty() {
                    out.insert(alias.clone(), targets);
                }
            }
        }
    }
    out
}
pub fn find_js_config(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let start = absolute(start);
    for dir in start.ancestors() {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let p = dir.join(name);
            if p.exists() {
                return Some((p, dir.to_path_buf()));
            }
        }
    }
    None
}
pub fn load_tsconfig_aliases(start: &Path) -> Aliases {
    let Some((p, dir)) = find_js_config(start) else {
        return Aliases::new();
    };
    let mut cache = ALIASES.lock().unwrap();
    cache
        .entry(p.clone())
        .or_insert_with(|| read_tsconfig_aliases(&p, &dir, &mut BTreeSet::new()))
        .clone()
}
pub fn load_tsconfig_base_url(start: &Path) -> Option<PathBuf> {
    let (p, dir) = find_js_config(start)?;
    let mut cache = BASES.lock().unwrap();
    cache
        .entry(p.clone())
        .or_insert_with(|| {
            read_json_config(&p).and_then(|d| {
                d["compilerOptions"]["baseUrl"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| norm(&dir.join(s)))
            })
        })
        .clone()
}
pub fn match_tsconfig_alias(raw: &str, pattern: &str) -> Option<((i64, i64), String, bool)> {
    let len = pattern.chars().count() as i64;
    if pattern.contains('*') {
        if pattern.matches('*').count() != 1 {
            return None;
        }
        let (pre, suf) = pattern.split_once('*')?;
        if !raw.starts_with(pre) || !raw.ends_with(suf) || raw.len() < pre.len() + suf.len() {
            return None;
        }
        return Some((
            (1, -(pre.chars().count() as i64)),
            raw[pre.len()..raw.len() - suf.len()].into(),
            true,
        ));
    }
    if raw == pattern {
        return Some(((0, -len), String::new(), false));
    }
    let pre = pattern.trim_end_matches('/');
    if !pre.is_empty() && raw.starts_with(&format!("{pre}/")) {
        return Some((
            (2, -(pre.chars().count() as i64)),
            raw[pre.len()..].trim_start_matches('/').into(),
            false,
        ));
    }
    None
}
pub fn resolve_tsconfig_alias(
    raw: &str,
    aliases: &Aliases,
    base: Option<&Path>,
) -> Option<PathBuf> {
    let best = aliases
        .iter()
        .filter_map(|(p, t)| match_tsconfig_alias(raw, p).map(|(s, c, w)| (s, c, w, t)))
        .min_by_key(|v| v.0);
    let Some((_, captured, wild, targets)) = best else {
        return base
            .map(|b| resolve_js_import_path(&b.join(raw)))
            .filter(|p| p.is_file());
    };
    let mut first = None;
    for target in targets {
        let p = if wild {
            PathBuf::from(if captured.is_empty() {
                target.clone()
            } else {
                target.replacen('*', &captured, 1)
            })
        } else {
            PathBuf::from(target).join(&captured)
        };
        let p = norm(&p);
        let resolved = resolve_js_import_path(&p);
        if resolved.is_file() {
            return Some(resolved);
        }
        if first.is_none() {
            first = Some(p);
        }
    }
    first
}
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let start = absolute(start);
    start
        .ancestors()
        .find(|p| {
            p.join("pnpm-workspace.yaml").exists()
                || read_json_config(&p.join("package.json"))
                    .is_some_and(|v| v.get("workspaces").is_some())
        })
        .map(Path::to_path_buf)
}
pub fn pnpm_workspace_globs(p: &Path) -> Vec<String> {
    let mut out = vec![];
    let mut inside = false;
    for raw in fsutil::read_text_lossy(p).unwrap_or_default().lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("packages:") {
            inside = true;
            continue;
        }
        if inside && line.starts_with('-') {
            let v = line[1..].trim().trim_matches(['\'', '"']);
            if !v.is_empty() && !v.starts_with('!') {
                out.push(v.into());
            }
            continue;
        }
        if inside && !raw.starts_with([' ', '\t']) {
            break;
        }
    }
    out
}
pub fn workspace_globs(root: &Path) -> Vec<String> {
    if root.join("pnpm-workspace.yaml").exists() {
        return pnpm_workspace_globs(&root.join("pnpm-workspace.yaml"));
    }
    let d = read_json_config(&root.join("package.json")).unwrap_or(Value::Null);
    let v = if d["workspaces"].is_array() {
        &d["workspaces"]
    } else {
        &d["workspaces"]["packages"]
    };
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.starts_with('!'))
        .map(str::to_string)
        .collect()
}
pub fn load_workspace_packages(start: &Path) -> BTreeMap<String, PathBuf> {
    let Some(root) = find_workspace_root(start) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for pat in workspace_globs(&root) {
        let paths: Vec<PathBuf> = if pat == "." || pat == "./" {
            vec![root.clone()]
        } else {
            glob::glob(&root.join(pat).to_string_lossy())
                .map(|g| g.flatten().collect())
                .unwrap_or_default()
        };
        for p in paths {
            if let Some(d) = read_json_config(&p.join("package.json"))
                && let Some(name) = d["name"].as_str().filter(|s| !s.is_empty())
            {
                out.insert(name.into(), p);
            }
        }
    }
    out
}
pub fn importer_platform(start: &Path) -> Option<&'static str> {
    for p in start.components().rev() {
        let s = p.as_os_str().to_string_lossy().to_lowercase();
        if ["native", "mobile", "ios", "android", "react-native"].contains(&s.as_str()) {
            return Some("native");
        }
        if ["web", "browser", "desktop", "electron"].contains(&s.as_str()) {
            return Some("web");
        }
    }
    None
}
pub fn platform_variants(p: &Path, platform: Option<&str>) -> Vec<PathBuf> {
    let mut variants = vec![];
    if let Some(p) = platform.filter(|p| ["web", "native", "ios", "android"].contains(p)) {
        variants.push(p);
    }
    for p in ["web", "native", "ios", "android"] {
        if !variants.contains(&p) {
            variants.push(p);
        }
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    variants
        .into_iter()
        .map(|v| p.with_file_name(format!("{stem}.{v}{ext}")))
        .collect()
}
pub fn resolve_export_targets(value: &Value, platform: Option<&str>) -> Vec<String> {
    if let Some(s) = value.as_str() {
        return vec![s.into()];
    }
    let mut out = vec![];
    if value.is_object() {
        let conditions = if platform == Some("native") {
            vec![
                "react-native",
                "source",
                "import",
                "module",
                "svelte",
                "require",
                "default",
                "types",
            ]
        } else {
            vec![
                "source", "import", "module", "svelte", "require", "default", "types",
            ]
        };
        for c in conditions {
            out.extend(resolve_export_targets(&value[c], platform));
        }
    }
    out
}
fn exports_candidates(dir: &Path, targets: Vec<String>, platform: Option<&str>) -> Vec<PathBuf> {
    let mut out = vec![];
    for s in targets {
        let p = dir.join(s);
        if absolute(&p).starts_with(absolute(dir)) {
            out.push(p.clone());
            out.extend(platform_variants(&p, platform));
        }
    }
    out
}
pub fn package_entry_candidates(dir: &Path, subpath: &str, platform: Option<&str>) -> Vec<PathBuf> {
    let d = read_json_config(&dir.join("package.json")).unwrap_or(Value::Null);
    let exports = &d["exports"];
    if !subpath.is_empty() {
        if let Some(obj) = exports.as_object() {
            let key = format!("./{subpath}");
            let targets = resolve_export_targets(&exports[&key], platform);
            if !targets.is_empty() {
                let c = exports_candidates(dir, targets, platform);
                if !c.is_empty() {
                    return c;
                }
            } else {
                for (pattern, value) in obj {
                    if pattern.matches('*').count() != 1 {
                        continue;
                    }
                    let (pre, suf) = pattern.split_once('*').unwrap();
                    if key.starts_with(pre)
                        && key.ends_with(suf)
                        && key.len() >= pre.len() + suf.len()
                    {
                        let cap = &key[pre.len()..key.len() - suf.len()];
                        let targets = resolve_export_targets(value, platform)
                            .into_iter()
                            .filter(|s| s.contains('*'))
                            .map(|s| s.replace('*', cap))
                            .collect();
                        let c = exports_candidates(dir, targets, platform);
                        if !c.is_empty() {
                            return c;
                        }
                    }
                }
            }
        }
        return vec![dir.join(subpath)];
    }
    if let Some(s) = exports.as_str() {
        return vec![dir.join(s)];
    }
    let targets = resolve_export_targets(&exports["."], platform);
    if !targets.is_empty() {
        return exports_candidates(dir, targets, platform);
    }
    let mut out = vec![];
    for k in ["svelte", "module", "main", "types"] {
        if let Some(s) = d[k].as_str() {
            out.push(dir.join(s));
        }
    }
    out.push(dir.join("src/index"));
    out.push(dir.join("index"));
    out
}
pub fn resolve_workspace_import(raw: &str, start: &Path) -> Option<PathBuf> {
    for (name, dir) in load_workspace_packages(start) {
        let sub = if raw == name {
            ""
        } else if let Some(s) = raw.strip_prefix(&format!("{name}/")) {
            s
        } else {
            continue;
        };
        // A built copy (`dist/`, `build/`, ...) is only a fallback for the
        // package's source entry (#3834).
        let mut build_fallback = None;
        for p in package_entry_candidates(&dir, sub, importer_platform(start)) {
            let p = resolve_js_import_path(&p);
            if source_file(&p) {
                if !build_output_path(&p, &dir) {
                    return Some(p);
                }
                build_fallback.get_or_insert(p);
            }
        }
        if build_fallback.is_some() {
            return build_fallback;
        }
    }
    None
}
const BUILD_OUTPUT_DIRS: &[&str] = &["build", "dist", "dist-protected", "out", "target"];
fn build_output_path(path: &Path, package_dir: &Path) -> bool {
    let path = crate::extractors::resolution::resolved(path);
    let dir = crate::extractors::resolution::resolved(package_dir);
    path.strip_prefix(&dir).is_ok_and(|rel| {
        rel.components()
            .next()
            .is_some_and(|c| BUILD_OUTPUT_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
    })
}
pub fn find_js_project_anchor(start: &Path) -> PathBuf {
    let p = absolute(start);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for q in p.ancestors() {
        if home.as_deref() == Some(q) {
            break;
        }
        if q.join("package.json").is_file() || q.join("pnpm-workspace.yaml").is_file() {
            return q.to_path_buf();
        }
    }
    for q in p.ancestors() {
        if [".git", ".hg", ".svn"].iter().any(|s| q.join(s).exists()) {
            return q.to_path_buf();
        }
    }
    p
}
pub fn load_package_imports(start: &Path) -> Option<(PathBuf, Value)> {
    let p = absolute(start);
    let mut cache = IMPORTS.lock().unwrap();
    cache
        .entry(p.clone())
        .or_insert_with(|| {
            for q in p.ancestors() {
                let manifest = q.join("package.json");
                if manifest.is_file() {
                    return read_json_config(&manifest).and_then(|d| {
                        d.get("imports")
                            .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
                            .map(|v| (q.to_path_buf(), v.clone()))
                    });
                }
            }
            None
        })
        .clone()
}
pub fn match_subpath_import(raw: &str, pattern: &str) -> Option<(i64, String, bool)> {
    if pattern.contains('*') {
        let ((_, score), capture, w) = match_tsconfig_alias(raw, pattern)?;
        Some((score, capture, w))
    } else if raw == pattern {
        Some((
            -(pattern.chars().count() as i64 + 1_000_000),
            String::new(),
            false,
        ))
    } else {
        None
    }
}
pub fn resolve_package_import(raw: &str, start: &Path) -> Option<PathBuf> {
    if !raw.starts_with('#') {
        return None;
    }
    let (dir, imports) = load_package_imports(start)?;
    let (_, capture, wild, v) = imports
        .as_object()?
        .iter()
        .filter(|(k, _)| k.starts_with('#'))
        .filter_map(|(k, v)| match_subpath_import(raw, k).map(|(s, c, w)| (s, c, w, v)))
        .min_by_key(|v| v.0)?;
    let mut target = resolve_export_targets(v, None).into_iter().next()?;
    if !target.starts_with("./") {
        return None;
    }
    if wild && !capture.is_empty() {
        target = target.replacen('*', &capture, 1);
    }
    let p = resolve_js_import_path(&dir.join(target));
    p.is_file().then_some(p)
}
pub fn resolve_js_module_path(raw: &str, start: Option<&Path>) -> Option<PathBuf> {
    let Some(start) = start else {
        return Some(resolve_js_import_path(Path::new(raw)));
    };
    if raw.starts_with('.') {
        return Some(resolve_js_import_path(&start.join(raw)));
    }
    let aliases = load_tsconfig_aliases(start);
    let base = load_tsconfig_base_url(start);
    if let Some(p) = resolve_tsconfig_alias(raw, &aliases, base.as_deref()) {
        return Some(resolve_js_import_path(&p));
    }
    if let Some(p) = resolve_package_import(raw, start) {
        return Some(p);
    }
    if let Some(p) = resolve_workspace_import(raw, start) {
        return Some(p);
    }
    // Unmapped `@/` project-root convention fallback (#3357). Active only when
    // no tsconfig.json/jsconfig.json exists in the upward tree.
    if let Some(sub) = raw.strip_prefix("@/").filter(|s| !s.is_empty())
        && find_js_config(start).is_none()
    {
        let anchor = find_js_project_anchor(start);
        for p in [anchor.join("src").join(sub), anchor.join(sub)] {
            let p = resolve_js_import_path(&p);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
pub fn resolve_js_import_target(raw: &str, source: &str) -> Option<(String, Option<PathBuf>)> {
    if raw.is_empty() {
        return None;
    }
    if let Some(p) = resolve_js_module_path(raw, Path::new(source).parent()) {
        return Some((make_id([p.to_string_lossy().as_ref()]), Some(p)));
    }
    if raw.ends_with('/') {
        return None;
    }
    let parts: Vec<_> = raw.split('/').collect();
    let package = if raw.starts_with('@') {
        parts.iter().take(2).copied().collect::<Vec<_>>().join("/")
    } else {
        parts[0].into()
    };
    Some((make_id(["ref", &package]), None))
}
pub fn resolve_lua_import_target(raw: &str, source: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let rel = raw.replace('.', "/");
    let start = Path::new(source).parent().unwrap_or(Path::new("."));
    for dir in start.ancestors().take(6) {
        for p in [
            dir.join(format!("{rel}.lua")),
            dir.join(format!("{rel}.luau")),
            dir.join(&rel).join("init.lua"),
            dir.join(&rel).join("init.luau"),
        ] {
            if p.is_file() {
                return make_id([p.to_string_lossy().as_ref()]);
            }
        }
    }
    make_id([raw])
}
pub fn vue_mask_non_script(src: &str) -> (String, Option<String>) {
    let re =
        regex::Regex::new(r#"(?i)(<script\b(?:"[^"]*"|'[^']*'|[^>"'])*>)([\s\S]*?)(</script\s*>)"#)
            .unwrap();
    let lang_re = regex::Regex::new(r#"(?i)\blang\s*=\s*['"]?([A-Za-z]+)['"]?"#).unwrap();
    let blank = |s: &str| {
        s.chars()
            .map(|c| if c == '\n' || c == '\r' { c } else { ' ' })
            .collect::<String>()
    };
    let (mut out, mut lang, mut pos) = (String::new(), None, 0);
    for cap in re.captures_iter(src) {
        let m = cap.get(0).unwrap();
        out.push_str(&blank(&src[pos..m.start()]));
        out.push_str(&blank(&cap[1]));
        out.push_str(&cap[2]);
        out.push_str(&blank(&cap[3]));
        pos = m.end();
        if lang.is_none() {
            lang = lang_re.captures(&cap[1]).map(|c| c[1].to_lowercase());
        }
    }
    out.push_str(&blank(&src[pos..]));
    (out, lang)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let action = r["action"].as_str().unwrap_or("");
    let text = r["text"].as_str().unwrap_or("");
    let path = Path::new(r["path"].as_str().unwrap_or("."));
    Ok(match action {
        "clear" => {
            clear();
            Value::Null
        }
        "strip_jsonc" => json!(strip_jsonc(text)),
        "resolve_js_import_path" => json!(resolve_js_import_path(path)),
        "read_json_config" => json!(read_json_config(path)),
        "find_js_config" => json!(find_js_config(path)),
        "aliases" => json!(load_tsconfig_aliases(path)),
        "base_url" => json!(load_tsconfig_base_url(path)),
        "workspace_root" => json!(find_workspace_root(path)),
        "workspace_globs" => json!(workspace_globs(path)),
        "workspace_packages" => json!(load_workspace_packages(path)),
        "platform" => json!(importer_platform(path)),
        "variants" => json!(platform_variants(path, r["platform"].as_str())),
        "export_targets" => json!(resolve_export_targets(&r["value"], r["platform"].as_str())),
        "package_entries" => json!(package_entry_candidates(path, text, r["platform"].as_str())),
        "workspace_import" => json!(resolve_workspace_import(text, path)),
        "anchor" => json!(find_js_project_anchor(path)),
        "package_imports" => json!(load_package_imports(path)),
        "package_import" => json!(resolve_package_import(text, path)),
        "module_path" => json!(resolve_js_module_path(text, Some(path))),
        "import_target" => json!(resolve_js_import_target(text, &path.to_string_lossy())),
        "lua_target" => json!(resolve_lua_import_target(text, &path.to_string_lossy())),
        "vue_mask" => json!(vue_mask_non_script(text)),
        "match_alias" => json!(match_tsconfig_alias(
            text,
            r["pattern"].as_str().unwrap_or("")
        )),
        "match_subpath" => json!(match_subpath_import(
            text,
            r["pattern"].as_str().unwrap_or("")
        )),
        "resolve_alias" => json!(resolve_tsconfig_alias(
            text,
            &serde_json::from_value(r["aliases"].clone()).map_err(crate::Error::from)?,
            r["base"].as_str().map(Path::new)
        )),
        _ => return Err(format!("Unknown JS resolution action: {action}").into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_candidates_skip_outputs_outside_the_corpus_and_scope_is_restored() {
        let _serial = super::super::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        // Plain on Windows (no `\\?\`), as the engine resolves paths.
        let root = crate::fsutil::plain(&temp.path().canonicalize().unwrap());
        let package = root.join("packages/lib");
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::create_dir_all(package.join("dist")).unwrap();
        std::fs::create_dir(root.join("app")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@test/lib","main":"./dist/index.js","types":"./dist/index.d.ts"}"#,
        )
        .unwrap();
        let source = package.join("src/index.ts");
        let built = package.join("dist/index.js");
        for path in [&source, &built, &package.join("dist/index.d.ts")] {
            std::fs::write(path, "export const value = 1;").unwrap();
        }
        clear();
        let importer = root.join("app");
        // Graphify 0.9.74 (#3834): with both on disk the source entry wins
        // over the built `dist/` copy, which stays the fallback.
        assert_eq!(
            resolve_workspace_import("@test/lib", &importer),
            Some(source.clone())
        );
        {
            let _scope = enter_source_corpus(Some(source_corpus(std::slice::from_ref(&built))));
            assert_eq!(
                resolve_workspace_import("@test/lib", &importer),
                Some(built.clone())
            );
            {
                let _explicit =
                    enter_source_corpus(Some(source_corpus(std::slice::from_ref(&source))));
                assert_eq!(
                    resolve_workspace_import("@test/lib", &importer),
                    Some(source.clone())
                );
            }
            assert_eq!(
                resolve_workspace_import("@test/lib", &importer),
                Some(built.clone())
            );
        }
        {
            let _both = enter_source_corpus(Some(source_corpus(&[source.clone(), built.clone()])));
            assert_eq!(
                resolve_workspace_import("@test/lib", &importer),
                Some(source.clone())
            );
        }
        assert_eq!(
            resolve_workspace_import("@test/lib", &importer),
            Some(source)
        );
    }
}
