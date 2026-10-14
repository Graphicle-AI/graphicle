// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/detect.py
//! Portable manifests with separate AST/semantic stamps and conservative mtimes.
use crate::{Result, cache, fsutil, paths, python_compat};
use md5::{Digest, Md5};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
pub fn os_path(path: &Path) -> String {
    #[cfg(not(windows))]
    {
        fsutil::path_string(path)
    }
    #[cfg(windows)]
    {
        let s = fsutil::path_string(path);
        if s.starts_with(r"\\?\") {
            return s;
        }
        let absolute = crate::detect_ignore::absolute_lexical(path)
            .to_string_lossy()
            .into_owned();
        if let Some(tail) = absolute.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{tail}")
        } else {
            format!(r"\\?\{absolute}")
        }
    }
}
pub fn md5_file(path: &Path) -> String {
    let hash = || -> std::io::Result<String> {
        let mut f = File::open(os_path(path))?;
        let mut h = Md5::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(format!("{:x}", h.finalize()))
    };
    hash().unwrap_or_default()
}
pub fn mtime(path: &Path) -> std::io::Result<f64> {
    let m = fs::metadata(os_path(path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(m.mtime() as f64 + m.mtime_nsec() as f64 * 1e-9)
    }
    #[cfg(not(unix))]
    {
        Ok(m.modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_else(|e| -e.duration().as_secs_f64()))
    }
}
pub fn stat_and_hash(path: &str) -> Option<(String, f64, String)> {
    let p = Path::new(path);
    Some((path.into(), mtime(p).ok()?, md5_file(p)))
}
pub fn to_relative_for_storage(key: &str, root: &Path) -> String {
    let path = Path::new(key);
    if !path.is_absolute() {
        return key.into();
    }
    let Ok(root) = fsutil::resolve_missing(root) else {
        return key.into();
    };
    let root = PathBuf::from(paths::nfc(&fsutil::path_string(&root)));
    let path = PathBuf::from(paths::nfc(&paths::normpath(&fsutil::path_string(path))));
    path.strip_prefix(root)
        .map(|p| fsutil::path_string(p).replace(std::path::MAIN_SEPARATOR, "/"))
        .unwrap_or_else(|_| key.into())
}
/// `_looks_absolute`: absolute under any platform's syntax, so a manifest
/// written on another OS keeps its foreign absolute keys unanchored (#1964).
fn looks_absolute(key: &str) -> bool {
    let bytes = key.as_bytes();
    Path::new(key).is_absolute()
        || key.starts_with(['/', '\\'])
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}
pub fn to_absolute_from_storage(key: &str, root: &Path) -> Result<String> {
    if looks_absolute(key) {
        return Ok(normpath_own_flavor(key));
    }
    // Only the key's own dot segments are normalized (#1964): a relative key
    // and an equivalent absolute key collapse to one string.
    Ok(paths::nfc(&paths::normpath(&fsutil::path_string(
        &fsutil::resolve_missing(root)
            .map_err(crate::Error::from)?
            .join(Path::new(key)),
    ))))
}
/// `_normpath_own_flavor` (0.9.71): dot-segment normalize an absolute key
/// under its own platform's syntax, so a foreign key stays readable there.
/// A drive letter or leading backslash is Windows (`ntpath.normpath`);
/// anything else not absolute here is POSIX (`posixpath.normpath`).
fn normpath_own_flavor(key: &str) -> String {
    if Path::new(key).is_absolute() {
        return if cfg!(windows) {
            ntpath_normpath(key)
        } else {
            paths::normpath(key)
        };
    }
    let bytes = key.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if key.starts_with('\\') || drive {
        ntpath_normpath(key)
    } else {
        paths::normpath(key)
    }
}
/// Python 3.12 `ntpath.splitroot` on a path whose `/` are already `\`.
fn ntpath_splitroot(p: &str) -> (&str, &str, &str) {
    let b = p.as_bytes();
    if b.first() == Some(&b'\\') {
        if b.get(1) == Some(&b'\\') {
            // UNC drives (\\server\share, \\?\UNC\server\share) and device
            // drives (\\.\device, \\?\device).
            let start = if p
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("\\\\?\\UNC\\"))
            {
                8
            } else {
                2
            };
            let Some(index) = p[start..].find('\\').map(|i| i + start) else {
                return (p, "", "");
            };
            let Some(index2) = p[index + 1..].find('\\').map(|i| i + index + 1) else {
                return (p, "", "");
            };
            return (&p[..index2], &p[index2..index2 + 1], &p[index2 + 1..]);
        }
        return ("", &p[..1], &p[1..]);
    }
    if let Some((colon, ':')) = p.char_indices().nth(1) {
        // `p[:2]` is two characters; the first may be non-ASCII.
        let two = colon + 1;
        if p[two..].starts_with('\\') {
            return (&p[..two], &p[two..two + 1], &p[two + 1..]);
        }
        return (&p[..two], "", &p[two..]);
    }
    ("", "", p)
}
/// Python 3.12 `ntpath.normpath` (the pure-Python implementation every
/// non-Windows host uses).
fn ntpath_normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (drive, root, rest) = ntpath_splitroot(&path);
    let prefix = format!("{drive}{root}");
    let mut comps: Vec<&str> = rest.split('\\').collect();
    let mut i = 0;
    while i < comps.len() {
        if comps[i].is_empty() || comps[i] == "." {
            comps.remove(i);
        } else if comps[i] == ".." {
            if i > 0 && comps[i - 1] != ".." {
                comps.drain(i - 1..=i);
                i -= 1;
            } else if i == 0 && !root.is_empty() {
                comps.remove(i);
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    if prefix.is_empty() && comps.is_empty() {
        comps.push(".");
    }
    format!("{prefix}{}", comps.join("\\"))
}
/// `_collapse_manifest_duplicates` (#3781): canonicalize each key; when two
/// raw keys collide, the more recently `seen` entry wins, and entries
/// without comparable `seen` values keep the historical last-wins order.
fn collapse_duplicates(
    items: impl IntoIterator<Item = (String, Value)>,
    canonical: impl Fn(&str) -> Option<String>,
) -> Map<String, Value> {
    let mut result = Map::new();
    for (key, value) in items {
        let Some(key) = canonical(&key) else {
            continue;
        };
        if let Some(previous) = result.get(&key) {
            let seen = |v: &Value| v.as_object().and_then(|o| o.get("seen")).and_then(numeric);
            if let (Some(previous), Some(new)) = (seen(previous), seen(&value))
                && new < previous
            {
                continue;
            }
        }
        result.insert(key, value);
    }
    result
}
/// `_manifest_storage_anchor` (0.9.77, #3785): a manifest kept in
/// `<repo>/.graphicle/` stores its keys relative to `<repo>` even when the
/// scan root is a subfolder, so runs on sibling subfolders share one key space.
pub fn manifest_storage_anchor(path: &Path, root: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return root.to_path_buf();
    }
    let Ok(manifest) = fsutil::resolve_missing(path) else {
        return root.to_path_buf();
    };
    let in_out = manifest
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == paths::DEFAULT_OUT_DIR || n == paths::OUT_DIR_NAME.as_str());
    if let Some(base) = manifest.parent().and_then(Path::parent).filter(|_| in_out)
        && let Ok(resolved) = fsutil::resolve_missing(root)
        && resolved != base
        && resolved.starts_with(base)
    {
        return base.to_path_buf();
    }
    root.to_path_buf()
}
pub fn load_manifest(path: &Path, root: Option<&Path>) -> Value {
    let Some(raw) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return json!({});
    };
    let Value::Object(raw) = raw else {
        return raw;
    };
    let anchor = root.map(|root| manifest_storage_anchor(path, root));
    Value::Object(collapse_duplicates(raw, |key| match &anchor {
        Some(anchor) => to_absolute_from_storage(key, anchor)
            .ok()
            .map(|k| paths::nfc(&k)),
        None => Some(paths::nfc(key)),
    }))
}
pub fn numeric(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
}
fn equivalent(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|other| equivalent(v, other)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equivalent(a, b))
        }
        _ => match (python_compat::scalar_key(a), python_compat::scalar_key(b)) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        },
    }
}
fn normalized_entry(value: &Value) -> Option<Value> {
    if numeric(value).is_some() {
        return Some(json!({"mtime":value,"ast_hash":"","semantic_hash":""}));
    }
    let object = value.as_object()?;
    if let Some(hash) = object
        .get("hash")
        .filter(|_| !object.contains_key("ast_hash"))
    {
        return Some(
            json!({"mtime":object.get("mtime").unwrap_or(&json!(0)),"ast_hash":hash,"semantic_hash":""}),
        );
    }
    Some(value.clone())
}
fn index(paths: Option<&[String]>) -> Option<HashSet<String>> {
    paths.map(|items| {
        items
            .iter()
            .flat_map(|p| [p.clone(), paths::nfc(p)])
            .collect()
    })
}
fn in_set(path: &str, set: Option<&HashSet<String>>) -> bool {
    // An empty set (the usual clear lists) holds nothing any spelling of the
    // path could match; skip resolving it on disk.
    let Some(set) = set.filter(|set| !set.is_empty()) else {
        return false;
    };
    if set.contains(path) || set.contains(&paths::nfc(path)) {
        return true;
    }
    fsutil::resolve_missing(Path::new(path)).is_ok_and(|p| {
        let p = fsutil::path_string(&p);
        set.contains(&p) || set.contains(&paths::nfc(&p))
    })
}
#[derive(Default, Clone)]
pub struct ManifestOptions {
    pub root: Option<PathBuf>,
    pub scan_corpus: Option<Vec<String>>,
    pub clear_semantic: Option<Vec<String>>,
    pub clear_ast: Option<Vec<String>>,
    pub now: Option<f64>,
}
/// `_checkout_prefix` (0.9.80): the longest directory prefix of `abs_key`
/// whose remaining suffix (forward slashes, NFC) is one of `relative_keys`.
/// Prefixes `""`, `"/"` and `"\\"` are skipped, and the prefix must be a real
/// directory.
fn checkout_prefix(abs_key: &str, relative_keys: &HashSet<String>) -> Option<String> {
    let normalized = paths::nfc(abs_key).replace('\\', "/");
    let parts = normalized.split('/').collect::<Vec<_>>();
    // Suffixes shrink as `i` grows, so the first one that qualifies is the longest.
    (1..parts.len()).find_map(|i| {
        if !relative_keys.contains(&parts[i..].join("/")) {
            return None;
        }
        let prefix = parts[..i].join("/");
        (!matches!(prefix.as_str(), "" | "/" | "\\") && Path::new(&prefix).is_dir())
            .then_some(prefix)
    })
}
/// `_previous_checkout_absolute_keys` (0.9.80, #3581): absolute keys that
/// still name a previous checkout. A key is a candidate when it stays absolute
/// against `storage_root`; candidates sharing one directory prefix are a
/// previous checkout when there are at least two of them. One absolute key is
/// an include row and stays.
fn previous_checkout_absolute_keys<'a>(
    existing_keys: impl IntoIterator<Item = &'a String>,
    storage_root: &Path,
    relative_keys: &HashSet<String>,
) -> HashSet<String> {
    let mut groups: HashMap<String, HashSet<String>> = HashMap::new();
    for key in existing_keys {
        if !looks_absolute(key) {
            continue;
        }
        let stored = paths::nfc(&to_relative_for_storage(key, storage_root));
        if !looks_absolute(&stored) {
            continue;
        }
        if let Some(prefix) = checkout_prefix(&stored, relative_keys) {
            groups.entry(prefix).or_default().insert(key.clone());
        }
    }
    groups
        .into_values()
        .filter(|keys| keys.len() >= 2)
        .flatten()
        .collect()
}
pub fn save_manifest(
    files: &Map<String, Value>,
    path: &Path,
    kind: &str,
    options: &ManifestOptions,
) -> Result<()> {
    // Another root may have activated different overrides in this thread.
    if let Some(root) = &options.root {
        crate::rcfile::activate_language_overrides(root);
    }
    let existing = load_manifest(path, options.root.as_deref());
    let existing = existing
        .as_object()
        .ok_or("Manifest must contain an object")?;
    let scan = index(options.scan_corpus.as_deref());
    let clear = index(options.clear_semantic.as_deref());
    let clear_ast = index(options.clear_ast.as_deref());
    let root = options
        .root
        .as_ref()
        .map(|r| fsutil::resolve_missing(r).unwrap_or_else(|_| r.clone()));
    let in_root = |p: &str| {
        root.as_ref().is_some_and(|r| {
            Path::new(p).starts_with(r)
                || fsutil::resolve_missing(Path::new(p)).is_ok_and(|p| p.starts_with(r))
        })
    };
    let all = files
        .values()
        .flat_map(|v| v.as_array().into_iter().flatten().filter_map(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let storage_root = options
        .root
        .as_ref()
        .map(|r| manifest_storage_anchor(path, r));
    // A subset save omits scan_corpus and must keep every live row it was not
    // given (#917), including an include row and a previous checkout: the
    // checkout drop runs only on a full scan (0.9.80, #3581).
    let stale_checkout = match (&scan, &storage_root) {
        (Some(_), Some(storage_root)) => {
            let new_rels = all
                .iter()
                .map(|f| paths::nfc(&to_relative_for_storage(&paths::nfc(f), storage_root)))
                .filter(|stored| !looks_absolute(stored))
                .collect::<HashSet<_>>();
            previous_checkout_absolute_keys(existing.keys(), storage_root, &new_rels)
        }
        _ => HashSet::new(),
    };
    let mut manifest = Map::new();
    for (p, entry) in existing {
        let Some(mut entry) = normalized_entry(entry) else {
            continue;
        };
        if !Path::new(p).exists() {
            continue;
        }
        if stale_checkout.contains(p) {
            continue; // a previous checkout copied with the output folder (#3581)
        }
        if scan.is_some() && !in_set(p, scan.as_ref()) && in_root(p) {
            continue;
        }
        if in_set(p, clear_ast.as_ref()) {
            entry["ast_hash"] = json!("");
            entry["semantic_hash"] = json!("");
        } else if in_set(p, clear.as_ref()) {
            entry["semantic_hash"] = json!("");
        }
        manifest.insert(p.clone(), entry);
    }
    let workers = if cfg!(target_os = "macos") {
        // Opening files from many threads contends on macOS (see `io_workers`).
        crate::par::io_workers()
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get() + 4)
            .unwrap_or(5)
            .min(32)
    }
    .min(all.len().max(1));
    // Files are claimed dynamically, so a few large ones do not idle the rest.
    let hashed = crate::par::map(&all, workers, |p| stat_and_hash(p))
        .into_iter()
        .flatten()
        .map(|(p, t, h)| (p, (t, h)))
        .collect::<HashMap<_, _>>();
    for p in all {
        let Some((mtime, hash)) = hashed.get(&p) else {
            continue;
        };
        let key = paths::nfc(&p);
        let prev =
            normalized_entry(existing.get(&key).unwrap_or(&Value::Null)).unwrap_or(json!({}));
        // Computed here: hash workers do not see this thread's overrides.
        let current_lang = crate::rcfile::cache_salt(&p);
        let lang_changed = prev.get("language").filter(|v| !v.is_null()).cloned()
            != current_lang.clone().map(Value::String);
        let ast = if matches!(kind, "ast" | "both") {
            json!(hash)
        } else if lang_changed {
            json!("")
        } else {
            prev.get("ast_hash").cloned().unwrap_or(json!(""))
        };
        let sem = if matches!(kind, "semantic" | "both") {
            json!(hash)
        } else if lang_changed {
            json!("")
        } else if prev.get("ast_hash") == Some(&json!(hash)) {
            prev.get("semantic_hash").cloned().unwrap_or(json!(""))
        } else {
            json!("")
        };
        let unchanged = numeric(&prev["seen"]).is_some()
            && numeric(&prev["mtime"]) == Some(*mtime)
            && (!matches!(kind, "ast" | "both")
                || equivalent(&ast, prev.get("ast_hash").unwrap_or(&json!(""))))
            && (!matches!(kind, "semantic" | "both")
                || equivalent(&sem, prev.get("semantic_hash").unwrap_or(&json!(""))))
            && !lang_changed
            && !in_set(&p, clear_ast.as_ref())
            && !in_set(&p, clear.as_ref());
        let seen = if unchanged {
            prev["seen"].clone()
        } else {
            json!(
                options
                    .now
                    .unwrap_or_else(|| cache::clock_ns() as f64 / 1e9)
            )
        };
        let mut entry = json!({"mtime":mtime,"seen":seen,"ast_hash":ast,"semantic_hash":sem});
        if let Some(language) = current_lang {
            // Without an override a row keeps the legacy shape.
            entry["language"] = json!(language);
        }
        manifest.insert(key, entry);
    }
    let manifest = Value::Object(collapse_duplicates(manifest, |k| {
        Some(paths::nfc(&match storage_root.as_ref() {
            Some(r) => to_relative_for_storage(k, r),
            None => k.to_owned(),
        }))
    }));
    if path.is_file()
        && fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|v| v.is_object() && equivalent(&v, &manifest))
    {
        return Ok(());
    }
    paths::write_json_atomic(path, &manifest, Some(2), true).map_err(crate::Error::from)
}
pub fn mtime_may_hide_a_rewrite(current: f64, stored: &Value) -> bool {
    let Some(seen) = numeric(&stored["seen"]) else {
        return false;
    };
    let delta = seen - current;
    delta >= 0.0 && delta < if current.fract() == 0.0 { 2.0 } else { 0.05 }
}
/// With a scan `root`, manifest entries outside it belong to another scan of a
/// shared manifest and are neither deleted nor excluded (0.9.77, #3785).
pub fn incremental_from_full(
    mut full: Value,
    manifest: &Value,
    kind: &str,
    root: Option<&Path>,
) -> Result<Value> {
    let files = full["files"].as_object().ok_or("Detection files missing")?;
    let empty = || Value::Object(files.keys().map(|k| (k.clone(), json!([]))).collect());
    if !crate::cache_paths::truth(manifest) {
        let new = full["files"].clone();
        let total = full["total_files"].clone();
        let unchanged = empty();
        let object = full.as_object_mut().unwrap();
        object.insert("incremental".into(), json!(true));
        object.insert("new_files".into(), new);
        object.insert("unchanged_files".into(), unchanged);
        object.insert("new_total".into(), total);
        object.insert("deleted_files".into(), json!([]));
        object.insert("excluded_files".into(), json!([]));
        return Ok(full);
    }
    let manifest = manifest
        .as_object()
        .ok_or("Manifest must contain an object")?;
    let mut new = empty();
    let mut unchanged = empty();
    let mut current_files = HashSet::new();
    let mut total = 0;
    for (ftype, files) in files {
        for p in files
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            current_files.insert(paths::nfc(p));
            let stored = manifest.get(&paths::nfc(p)).unwrap_or(&Value::Null);
            let current = mtime(Path::new(p)).unwrap_or(0.0);
            // A language change invalidates even identical bytes and legacy
            // rows (0.9.87, #2961).
            let stored_lang = stored.get("language").filter(|v| !v.is_null()).cloned();
            let changed = if !stored.is_null()
                && crate::rcfile::cache_salt(p).map(Value::String) != stored_lang
            {
                true
            } else if let Some(n) = numeric(stored) {
                current != n
            } else if stored.is_object() {
                let stored = normalized_entry(stored).unwrap();
                let hash_key = if kind == "semantic" {
                    "semantic_hash"
                } else {
                    "ast_hash"
                };
                let hash = &stored[hash_key];
                if !crate::cache_paths::truth(hash) {
                    true
                } else {
                    let old = if stored["mtime"].is_object() {
                        numeric(&stored["mtime"]["mtime"])
                    } else {
                        numeric(&stored["mtime"])
                    };
                    if old != Some(current) || mtime_may_hide_a_rewrite(current, &stored) {
                        json!(md5_file(Path::new(p))) != *hash
                    } else {
                        false
                    }
                }
            } else {
                true
            };
            if changed {
                new[ftype].as_array_mut().unwrap().push(json!(p));
                total += 1;
            } else {
                unchanged[ftype].as_array_mut().unwrap().push(json!(p));
            }
        }
    }
    let root = root.map(|r| fsutil::resolve_missing(r).unwrap_or_else(|_| r.to_path_buf()));
    let in_root = |p: &str| {
        root.as_ref().is_none_or(|r| {
            Path::new(p).starts_with(r)
                || fsutil::resolve_missing(Path::new(p)).is_ok_and(|p| p.starts_with(r))
        })
    };
    let mut deleted = Vec::new();
    let mut excluded = Vec::new();
    for p in manifest.keys() {
        if in_root(p) && !current_files.contains(&paths::nfc(p)) {
            if Path::new(p).exists() {
                excluded.push(p.clone());
            } else {
                deleted.push(p.clone());
            }
        }
    }
    let object = full.as_object_mut().unwrap();
    object.insert("incremental".into(), json!(true));
    object.insert("new_files".into(), new);
    object.insert("unchanged_files".into(), unchanged);
    object.insert("new_total".into(), json!(total));
    object.insert("deleted_files".into(), json!(deleted));
    object.insert("excluded_files".into(), json!(excluded));
    Ok(full)
}

#[cfg(test)]
mod own_flavor_tests {
    use super::{normpath_own_flavor, ntpath_normpath};

    #[test]
    fn ntpath_normpath_matches_python_3_12() {
        // Expected values printed by CPython 3.12.13 `ntpath.normpath`.
        for (input, expected) in [
            (r"C:\a\..\b", r"C:\b"),
            (r"C:/a/./b//c", r"C:\a\b\c"),
            ("C:\\", "C:\\"),
            ("C:", "C:"),
            (r"C:x\..\..\y", r"C:..\y"),
            (r"\\server\share\a\..\b", r"\\server\share\b"),
            (r"\\server\share", r"\\server\share"),
            (r"\\server", r"\\server"),
            (r"\\?\UNC\srv\sh\x\..\y", r"\\?\UNC\srv\sh\y"),
            (r"\\.\dev\a", r"\\.\dev\a"),
            (r"\a\..\..\b", r"\b"),
            (r"\..\x", r"\x"),
            ("\\", "\\"),
            (r"C:\..\a", r"C:\a"),
            ("c:/Data/Name/./proj/../f.py", r"c:\Data\Name\f.py"),
            (r"\\?\C:\a\.\b", r"\\?\C:\a\b"),
        ] {
            assert_eq!(ntpath_normpath(input), expected, "{input}");
        }
    }

    #[test]
    fn foreign_keys_keep_their_own_path_syntax() {
        // A Windows key read on POSIX is normalized as Windows (#3879); a POSIX
        // key stays POSIX.
        if !cfg!(windows) {
            assert_eq!(normpath_own_flavor(r"C:\proj\sub\..\a.py"), r"C:\proj\a.py");
            assert_eq!(normpath_own_flavor("/home/u/./x/../a.py"), "/home/u/a.py");
        }
    }
}
