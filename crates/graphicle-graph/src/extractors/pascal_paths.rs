// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/pascal.py, graphify/extractors/resolution.py
//! Pascal-specific path resolution from resolution.py. This does not claim
//! completion of the remaining JavaScript/Python/C resolvers in that file.
use super::base::{file_stem, make_id};
use crate::{fsutil, path_syntax};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};
type Cache = HashMap<String, HashMap<String, String>>;
static UNITS: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CLASSES: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
fn parent(path: &Path) -> PathBuf {
    let mut parts = path_syntax::posix_parts(&fsutil::path_string(path));
    if !path_syntax::posix_name(&parts).is_empty() {
        parts.pop();
    }
    PathBuf::from(path_syntax::posix_join(&parts))
}
pub fn project_root(path: &Path) -> PathBuf {
    let mut current = parent(path);
    let mut best = current.clone();
    for _ in 0..12 {
        if path_syntax::posix_parts(&fsutil::path_string(&current)).len() <= 1 {
            break;
        }
        let entries = std::fs::read_dir(&current)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect::<Vec<_>>();
        let count = |ext| {
            entries
                .iter()
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
                .count()
        };
        if count("pas") >= 2 || count("dpr") >= 1 {
            best = current.clone();
        }
        let next = parent(&current);
        if next == current {
            break;
        }
        current = next;
    }
    best
}
fn recursive_files(root: &Path, ext: &str) -> Vec<PathBuf> {
    // pathlib's recursive selector visits each directory once and does not
    // recurse through symlink directories. Preserve scandir order.
    let mut directories = vec![root.to_path_buf()];
    let mut selected = Vec::new();
    while let Some(dir) = directories.pop() {
        let entries = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        for entry in &entries {
            if entry.path().extension().and_then(|s| s.to_str()) == Some(ext) {
                selected.push(entry.path());
            }
        }
        for entry in entries {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                directories.push(entry.path());
            }
        }
    }
    selected
}
fn mapping(root: &Path, classes: bool) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for ext in if classes {
        &["pas", "pp", "dpr", "dpk"][..]
    } else {
        &["pas", "pp", "dpr", "dpk", "inc"][..]
    } {
        for file in recursive_files(root, ext) {
            let stem = file
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            out.insert(
                stem,
                if classes {
                    file_stem(&file)
                } else {
                    make_id([fsutil::path_string(&file).as_str()])
                },
            );
        }
    }
    out
}
pub fn resolve_unit(path: &Path, name: &str) -> String {
    let root = project_root(path);
    let key = fsutil::path_string(&root);
    let mut cache = UNITS.lock().unwrap();
    let map = cache.entry(key).or_insert_with(|| mapping(&root, false));
    map.get(&name.to_lowercase())
        .cloned()
        .unwrap_or_else(|| make_id([name]))
}
pub fn resolve_class(path: &Path, name: &str) -> Option<String> {
    let unit = name.strip_prefix(['T', 'I']).unwrap_or(name);
    let root = project_root(path);
    let key = fsutil::path_string(&root);
    let mut cache = CLASSES.lock().unwrap();
    let map = cache.entry(key).or_insert_with(|| mapping(&root, true));
    map.get(&unit.to_lowercase())
        .filter(|s| !s.is_empty())
        .map(|stem| make_id([stem.as_str(), name]))
}
pub fn clear() {
    UNITS.lock().unwrap().clear();
    CLASSES.lock().unwrap().clear();
}
