// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cache.py, graphify/extract.py
//! Portable source and identifier transforms from graphify/cache.py.
use crate::{Result, fsutil, ids, paths, python_compat};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};
pub const ROOT_MARKER: &str = "$graphicle-root$";
pub fn truth(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
pub fn ar(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn ps(p: &Path) -> String {
    fsutil::path_string(p)
}
pub fn resolve(p: &Path) -> Result<PathBuf> {
    fsutil::resolve_missing(p).map_err(crate::Error::from)
}
pub fn normalize(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().replace('/', "\\");
        PathBuf::from(s.strip_prefix("\\\\?\\").unwrap_or(&s).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        p.into()
    }
}
pub fn absolute(p: &Path) -> Result<PathBuf> {
    let p = if p.is_absolute() {
        p.into()
    } else {
        std::env::current_dir().map_err(crate::Error::from)?.join(p)
    };
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from(paths::normpath(&p.to_string_lossy())))
    }
    #[cfg(windows)]
    {
        let mut out = PathBuf::new();
        for part in p.components() {
            match part {
                std::path::Component::CurDir => (),
                std::path::Component::ParentDir => {
                    out.pop();
                }
                _ => out.push(part.as_os_str()),
            }
        }
        Ok(out)
    }
}
pub fn relative(path: &Path, base: &Path) -> Result<String> {
    let p = absolute(path)?;
    let b = absolute(base)?;
    let p = p.components().collect::<Vec<_>>();
    let b = b.components().collect::<Vec<_>>();
    if p.first() != b.first() {
        return Err("Paths are on different roots".into());
    }
    let common = p.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..")
    }
    for part in p.iter().skip(common) {
        out.push(part.as_os_str())
    }
    Ok(ps(&out))
}
fn escapes(rel: &str) -> bool {
    rel == ".." || rel.starts_with("../") || rel.starts_with("..\\")
}
pub fn stat_relative(key: &str, anchor: &Path) -> String {
    if !Path::new(key).is_absolute() {
        return key.into();
    }
    match relative(Path::new(key), anchor) {
        Ok(r) if !escapes(&r) => r.replace(std::path::MAIN_SEPARATOR, "/"),
        _ => key.into(),
    }
}
pub fn stat_absolute(key: &str, anchor: &Path) -> String {
    let p = Path::new(key);
    ps(&if p.is_absolute() {
        p.into()
    } else {
        anchor.join(p)
    })
}
pub fn normalize_source(src: &str, root: &Path) -> String {
    let s = src.replace('\\', "/");
    if !Path::new(&s).is_absolute() {
        return s;
    }
    match relative(Path::new(&s), root) {
        Ok(r) if !escapes(&r) => r.replace(std::path::MAIN_SEPARATOR, "/"),
        _ => s,
    }
}
pub fn relativize_sources(payload: &mut Value, root: &Path) {
    let Ok(root) = resolve(root) else { return };
    for bucket in ["nodes", "edges", "hyperedges", "raw_calls"] {
        if let Some(items) = payload.get_mut(bucket).and_then(Value::as_array_mut) {
            for item in items {
                if !item.is_object() {
                    continue;
                }
                for key in ["source_file", "definition_file"] {
                    let Some(source) = item.get(key).filter(|v| truth(v)).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let mut path = PathBuf::from(source);
                    if !path.is_absolute() {
                        let Ok(cwd) = absolute(&path) else { continue };
                        if cwd == root.join(&path) || !cwd.exists() {
                            continue;
                        }
                        path = cwd;
                    }
                    if let Ok(rel) = relative(&path, &root)
                        && !escapes(&rel)
                    {
                        item[key] = json!(rel.replace(std::path::MAIN_SEPARATOR, "/"));
                    }
                }
            }
        }
    }
}
pub fn absolutize_sources(payload: &mut Value, root: &Path) {
    let Ok(root) = resolve(root) else { return };
    absolutize_sources_at(payload, &root);
}
fn absolutize_sources_at(payload: &mut Value, root: &Path) {
    for bucket in ["nodes", "edges", "hyperedges", "raw_calls"] {
        if let Some(items) = payload.get_mut(bucket).and_then(Value::as_array_mut) {
            for item in items {
                if !item.is_object() {
                    continue;
                }
                for key in ["source_file", "definition_file"] {
                    let Some(src) = item.get(key).filter(|v| truth(v)).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if !Path::new(src).is_absolute() {
                        item[key] = json!(ps(&root.join(src)))
                    }
                }
            }
        }
    }
}
pub fn semantic_matches(result: &Value, path: &Path, root: &Path) -> Result<bool> {
    let expected = normalize(&absolute(path)?);
    for bucket in ["nodes", "edges", "hyperedges"] {
        for item in ar(&result[bucket]) {
            let Some(source) = item
                .get("source_file")
                .filter(|v| truth(v))
                .and_then(Value::as_str)
            else {
                continue;
            };
            // Portable IDs can also encode source_file when a walked root is
            // a symlink. Validate the restored path, before accepting the hit.
            let source = source
                .strip_prefix(ROOT_MARKER)
                .and_then(|suffix| suffix.strip_prefix('/'))
                .unwrap_or(source);
            let path = Path::new(source);
            let path = if path.is_absolute() {
                path.into()
            } else {
                root.join(path)
            };
            if normalize(&absolute(&path)?) != expected {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
pub fn id_anchor(path: &str, rel: &str) -> String {
    let full = ids::normalize_id(path);
    let tail = ids::normalize_id(rel);
    if tail.is_empty() || full == tail {
        return String::new();
    }
    full.strip_suffix(&format!("_{tail}")).unwrap_or("").into()
}

fn walked_root(path: &Path, resolved_root: &Path) -> Option<PathBuf> {
    let walked = absolute(path).ok()?;
    let parent = walked.parent()?;
    let resolved_parent = resolve(parent).ok()?;
    if let Ok(suffix) = resolved_parent.strip_prefix(resolved_root)
        && parent.ends_with(suffix)
    {
        let mut anchor = parent.to_path_buf();
        for _ in suffix.components() {
            anchor.pop();
        }
        return Some(anchor);
    }
    // A directory symlink inside the corpus can change the relative suffix.
    // Find the enclosing corpus without following the final file symlink:
    // its walked name is a distinct source even when the contents are shared.
    parent
        .ancestors()
        .find(|p| resolve(p).is_ok_and(|p| p == resolved_root))
        .map(Path::to_path_buf)
}

pub fn anchors(path: &Path, root: &Path) -> (Vec<String>, String, Vec<String>, String) {
    let Ok(resolved_root) = resolve(root) else {
        return (vec![], String::new(), vec![], String::new());
    };
    let resolved = resolve(path).unwrap_or(path.into());
    let rel = relative(&resolved, &resolved_root).unwrap_or_default();
    let walked = walked_root(path, &resolved_root);
    let mut given = id_anchor(&ps(path), &rel);
    if given.is_empty()
        && path.is_absolute()
        && let Some(anchor) = &walked
    {
        given = ids::normalize_id(&ps(anchor));
    }
    let actual = id_anchor(&ps(&resolved), &rel);
    let root_id = ids::normalize_id(&ps(&resolved_root));
    let restore = [&given, &actual, &root_id]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();
    let mut id_anchors = vec![given, actual, root_id];
    if root.is_absolute() {
        id_anchors.push(ids::normalize_id(&ps(root)));
    }
    id_anchors.retain(|s| !s.is_empty());
    id_anchors.sort();
    id_anchors.dedup();
    id_anchors.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    let mut path_anchors = [ps(&resolved_root), ps(root)]
        .into_iter()
        .filter(|s| Path::new(s).is_absolute())
        .collect::<Vec<_>>();
    // The extraction facade canonicalizes its root, while a caller can pass
    // files through a directory symlink (macOS /var -> /private/var is one).
    // IDs already recognize that walked prefix above; source_file must also
    // be portable, or a shared cache restores new IDs beside an old source
    // path and corpus normalization can no longer recognize those symbols.
    // Restore the source spelling alongside the ID spelling. Resolving the
    // parent above preserves a final file symlink's own name and source ID.
    let mut path_restore = ps(&resolved_root);
    if let Some(anchor) = walked {
        path_restore = ps(&anchor);
        path_anchors.push(path_restore.clone());
    }
    path_anchors.sort();
    path_anchors.dedup();
    path_anchors.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    (id_anchors, restore, path_anchors, path_restore)
}

#[cfg(all(test, unix))]
mod directory_alias_tests {
    use super::*;

    #[test]
    fn canonical_root_and_walked_files_restore_together_in_another_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let relocated = temp.path().join("relocated");
        for root in [&original, &relocated] {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/a.py"), "def alpha(): return 1\n").unwrap();
        }
        let alias = temp.path().join("walked");
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let walked_file = alias.join("src/a.py");
        let canonical_root = original.canonicalize().unwrap();
        let node = ids::make_id([
            walked_file.with_extension("").to_string_lossy().as_ref(),
            "alpha",
        ]);
        let mut record = json!({"nodes":[{"id":node,"source_file":ps(&walked_file)}],
            "edges":[],"raw_calls":[{"caller_nid":node,"source_file":ps(&walked_file)}]});
        relativize_sources(&mut record, &canonical_root);
        relativize_ids(&mut record, &walked_file, &canonical_root);
        assert_eq!(
            record["nodes"][0]["source_file"],
            format!("{ROOT_MARKER}/src/a.py")
        );
        assert_eq!(
            record["nodes"][0]["id"],
            format!("{ROOT_MARKER}_src_a_alpha")
        );
        let new_root = relocated.canonicalize().unwrap();
        absolutize_ids(&mut record, &new_root.join("src/a.py"), &new_root);
        absolutize_sources(&mut record, &new_root);
        assert_eq!(
            record["nodes"][0]["source_file"],
            ps(&new_root.join("src/a.py"))
        );
        assert_eq!(
            record["raw_calls"][0]["source_file"],
            record["nodes"][0]["source_file"]
        );
        assert!(!record.to_string().contains("walked"));
        assert!(!record.to_string().contains("original"));
    }
}
pub fn rewrite_strings(payload: &mut Value, f: &impl Fn(&str) -> String) {
    match payload {
        Value::Array(a) => {
            for v in a {
                if let Value::String(s) = v {
                    *s = f(s)
                } else {
                    rewrite_strings(v, f)
                }
            }
        }
        Value::Object(m) => {
            for v in m.values_mut() {
                if let Value::String(s) = v {
                    *s = f(s)
                } else {
                    rewrite_strings(v, f)
                }
            }
        }
        _ => (),
    }
}
pub fn rewrite_table_keys(payload: &mut Value, f: &impl Fn(&str) -> String) {
    if let Some(tables) = payload
        .get_mut("objc_field_types")
        .and_then(|v| v.get_mut("tables"))
        .and_then(Value::as_object_mut)
    {
        *tables = std::mem::take(tables)
            .into_iter()
            .map(|(k, v)| (f(&k), v))
            .collect();
    }
}
pub fn relativize_ids(payload: &mut Value, path: &Path, root: &Path) {
    let (ids, _, paths, _) = anchors(path, root);
    // The prefixes, built once instead of for every string and anchor.
    let path_prefixes = paths
        .iter()
        .map(|a| (a, [format!("{a}/"), format!("{a}\\")]))
        .collect::<Vec<_>>();
    let id_prefixes = ids.iter().map(|a| format!("{a}_")).collect::<Vec<_>>();
    let f = |s: &str| {
        for (a, seps) in &path_prefixes {
            if s == a.as_str() {
                return ROOT_MARKER.into();
            }
            for prefix in seps {
                if let Some(tail) = s.strip_prefix(prefix.as_str()) {
                    return format!("{ROOT_MARKER}/{}", tail.replace('\\', "/"));
                }
            }
        }
        for prefix in &id_prefixes {
            if let Some(tail) = s.strip_prefix(prefix.as_str()) {
                return format!("{ROOT_MARKER}_{tail}");
            }
        }
        s.into()
    };
    rewrite_strings(payload, &f);
    rewrite_table_keys(payload, &f);
}
pub fn absolutize_ids(payload: &mut Value, path: &Path, root: &Path) {
    let (_, id, _, path) = anchors(path, root);
    absolutize_ids_at(payload, &id, &path);
}
/// Restore marked and ordinary relative paths with the same walked spelling.
/// Resolving the root a second time would turn a warm directory-alias hit into
/// a different source from a fresh extraction through that alias.
pub(crate) fn restore_entry_paths(payload: &mut Value, path: &Path, root: &Path) {
    let (_, id, _, walked) = anchors(path, root);
    absolutize_ids_at(payload, &id, &walked);
    absolutize_sources_at(payload, Path::new(&walked));
}
fn absolutize_ids_at(payload: &mut Value, id: &str, path: &str) {
    let f = |s: &str| {
        let Some(tail) = s.strip_prefix(ROOT_MARKER) else {
            return s.into();
        };
        if tail.is_empty() {
            path.to_string()
        } else if let Some(tail) = tail.strip_prefix('/') {
            ps(&Path::new(path).join(tail))
        } else if let Some(rest) = tail.strip_prefix('_') {
            if id.is_empty() {
                rest.into()
            } else {
                format!("{id}{tail}")
            }
        } else {
            s.into()
        }
    };
    rewrite_strings(payload, &f);
    rewrite_table_keys(payload, &f);
}
pub struct SourceMatcher {
    pub walked: PathBuf,
    pub resolved: PathBuf,
}
impl SourceMatcher {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            walked: normalize(&absolute(root)?),
            resolved: normalize(&resolve(root)?),
        })
    }
    pub fn normalized(&self, src: &str) -> String {
        let mut n = normalize_source(src, &self.walked);
        if Path::new(&n).is_absolute() && self.walked != self.resolved {
            n = normalize_source(src, &self.resolved)
        }
        n
    }
    pub fn identity(&self, src: &str) -> Result<PathBuf> {
        let mut p = PathBuf::from(src);
        if !p.is_absolute() {
            p = self.walked.join(p)
        } else if self.walked != self.resolved {
            let normalized = normalize(&absolute(&p)?);
            if let Ok(rel) = normalized.strip_prefix(&self.resolved) {
                p = self.walked.join(rel)
            }
        }
        Ok(normalize(&absolute(&p)?))
    }
}
fn hash_key(v: &Value) -> Option<crate::python_compat::ScalarKey> {
    python_compat::scalar_key(v)
}
pub fn scope(
    result: &mut Value,
    root: &Path,
    allowed: Option<&[String]>,
) -> Result<(Vec<String>, usize)> {
    let Some(allowed) = allowed else {
        return Ok((vec![], 0));
    };
    let matcher = SourceMatcher::new(root)?;
    let allowed = allowed
        .iter()
        .map(|p| matcher.identity(p))
        .collect::<Result<HashSet<_>>>()?;
    let mut dropped_files = HashSet::new();
    let mut dropped = 0;
    let mut dropped_ids = BTreeSet::new();
    let mut kept_ids = BTreeSet::new();
    for bucket in ["nodes", "edges", "hyperedges"] {
        let mut kept = vec![];
        for item in ar(&result[bucket]) {
            if let Some(src) = item
                .get("source_file")
                .filter(|v| truth(v))
                .and_then(Value::as_str)
            {
                let display = matcher.normalized(src);
                if !allowed.contains(&matcher.identity(&display)?) {
                    dropped_files.insert(display);
                    dropped += 1;
                    if bucket == "nodes"
                        && !item["id"].is_null()
                        && let Some(k) = hash_key(&item["id"])
                    {
                        dropped_ids.insert(k);
                    }
                    continue;
                }
            }
            if bucket == "nodes"
                && !item["id"].is_null()
                && let Some(k) = hash_key(&item["id"])
            {
                kept_ids.insert(k);
            }
            kept.push(item.clone());
        }
        result[bucket] = json!(kept);
    }
    dropped_ids.retain(|id| !kept_ids.contains(id));
    prune_dangling(result, &dropped_ids);
    let mut dropped_files = dropped_files.into_iter().collect::<Vec<_>>();
    dropped_files.sort();
    Ok((dropped_files, dropped))
}
pub fn prune_dangling(result: &mut Value, ids: &BTreeSet<python_compat::ScalarKey>) {
    if ids.is_empty() {
        return;
    }
    if let Some(edges) = result.get_mut("edges").and_then(Value::as_array_mut) {
        edges.retain(|e| {
            let Some(a) = hash_key(&e["source"]) else {
                return true;
            };
            if ids.contains(&a) {
                return false;
            }
            hash_key(&e["target"]).is_none_or(|b| !ids.contains(&b))
        });
    }
    if let Some(hyper) = result.get_mut("hyperedges").and_then(Value::as_array_mut) {
        hyper.retain(|h| {
            let Some(members) = ar(&h["nodes"])
                .iter()
                .map(hash_key)
                .collect::<Option<Vec<_>>>()
            else {
                return true;
            };
            !members.iter().any(|id| ids.contains(id))
        });
    }
}
