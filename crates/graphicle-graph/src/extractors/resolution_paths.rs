// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/objc.py, graphify/extractors/resolution.py
//! Bounded realpath cache, quoted C include resolution and the declared
//! language suffixes (`.graphiclerc`) the path probes honour.
use crate::{fsutil, path_syntax, rcfile::effective_suffix};
use lru::LruCache;
use std::{
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};
type Paths = LruCache<PathBuf, PathBuf>;
/// The cache is split by key so extraction workers resolving different paths
/// do not queue on one lock. It only memoises lookups, so how entries are
/// grouped or evicted cannot change a result.
const SHARDS: usize = 16;
static PATHS: LazyLock<[Mutex<Paths>; SHARDS]> = LazyLock::new(|| {
    std::array::from_fn(|_| Mutex::new(LruCache::new(NonZeroUsize::new(65536 / SHARDS).unwrap())))
});
pub fn resolve_cached(path: &Path) -> io::Result<PathBuf> {
    // Absolute paths are independent of the process's working directory. On
    // macOS, asking for it on every cache hit performs filesystem syscalls.
    // Relative paths still include the current directory in their identity.
    let absolute;
    let key = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()?.join(path);
        &absolute
    };
    // A verbatim key (`\\?\C:\x`, or `//?/C:/x` from an older cache) is
    // looked up under its plain spelling, as `resolve_missing` returns it.
    #[cfg(windows)]
    let plain = fsutil::plain(key);
    #[cfg(windows)]
    let key = plain.as_path();
    resolve_key(key)
}
/// Resolve an absolute `key`, memoising every directory prefix so sibling
/// files share the symlink checks of their common ancestors.
fn resolve_key(key: &Path) -> io::Result<PathBuf> {
    if let Some(path) = lock(key)?.get(key) {
        return Ok(path.clone());
    }
    let components = key.components().collect::<Vec<_>>();
    let parent = match components.split_last() {
        Some((tail, parents)) if !parents.is_empty() => {
            Some((parents.iter().collect::<PathBuf>(), tail))
        }
        _ => None,
    };
    // Stop when the parent is not shorter: on Windows a UNC share
    // (`\\server\share`) yields `Prefix, RootDir` and is its own parent.
    // On Unix a parent is always shorter.
    let resolved = match parent {
        Some((parent, tail)) if parent.as_os_str().len() < key.as_os_str().len() => {
            let base = resolve_key(&parent)?;
            fsutil::resolve_missing_from(&base, Path::new(tail.as_os_str()))?
        }
        _ => fsutil::resolve_missing(key)?,
    };
    lock(key)?.put(key.to_path_buf(), resolved.clone());
    Ok(resolved)
}
fn lock(key: &Path) -> io::Result<std::sync::MutexGuard<'static, Paths>> {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    key.as_os_str().as_encoded_bytes().hash(&mut hasher);
    PATHS[hasher.finish() as usize % SHARDS]
        .lock()
        .map_err(|_| io::Error::other("path cache poisoned"))
}
pub fn clear_paths() {
    for shard in PATHS.iter() {
        if let Ok(mut cache) = shard.lock() {
            cache.clear();
        }
    }
}
/// `_declared_language_suffixes`: the extensions the active language
/// overrides map onto one of `targets`, sorted. A key must be a single
/// suffix (`Path("x" + key).suffix == key`), never a path fragment.
pub fn declared_language_suffixes(targets: &[&str]) -> Vec<String> {
    let mut suffixes = crate::rcfile::get_language_overrides()
        .into_iter()
        .filter(|(source, target)| {
            targets.contains(&target.as_str()) && {
                let probe = format!("x{source}");
                let name = Path::new(&probe)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                path_syntax::suffix(&name) == source
            }
        })
        .map(|(source, _)| source)
        .collect::<Vec<_>>();
    suffixes.sort();
    suffixes
}
/// `_is_python_package_dir`: `directory` holds an `__init__` initializer of
/// one of `suffixes` that is still that language, or of a suffix the
/// project declares as one of them.
pub fn is_python_package_dir(directory: &Path, suffixes: &[&str]) -> bool {
    let init = |suffix: &str| directory.join(format!("__init__{suffix}"));
    if suffixes.iter().any(|suffix| {
        let path = init(suffix);
        path.is_file() && suffixes.contains(&effective_suffix(&path).as_str())
    }) {
        return true;
    }
    declared_language_suffixes(suffixes)
        .iter()
        .filter(|suffix| !suffixes.contains(&suffix.as_str()))
        .any(|suffix| init(suffix).is_file())
}
/// `_probe_declared_python_module_candidate`: a package initializer or a
/// module file with a suffix the project declares as Python.
pub fn probe_declared_python_module_candidate(candidate: &Path) -> Option<PathBuf> {
    for suffix in declared_language_suffixes(&[".py"]) {
        let init = candidate.join(format!("__init__{suffix}"));
        if init.is_file() {
            return Some(init);
        }
        if candidate.file_name().is_some() {
            let module = candidate.with_extension(suffix.trim_start_matches('.'));
            if module.is_file() {
                return Some(module);
            }
        }
    }
    None
}
pub fn resolve_c_include_path(raw: &str, source: &str) -> io::Result<Option<PathBuf>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut parts = path_syntax::posix_parts(source);
    if !path_syntax::posix_name(&parts).is_empty() {
        parts.pop();
    }
    let candidate = resolve_cached(&PathBuf::from(path_syntax::posix_join(&parts)).join(raw))?;
    Ok(candidate.is_file().then_some(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_keys_resolve_through_their_parents() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pkg/sub/a.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        let root = fsutil::resolve_missing(dir.path()).unwrap();
        assert_eq!(resolve_cached(&file).unwrap(), root.join("pkg/sub/a.py"));
        let missing = root.join("pkg/missing/b.py");
        assert_eq!(resolve_cached(&missing).unwrap(), missing);
    }

    /// UNC, WSL and verbatim roots used to recurse without end (a UNC share
    /// is its own parent). They must return, whatever the network says.
    #[cfg(windows)]
    #[test]
    fn unc_wsl_and_verbatim_roots_terminate() {
        for root in [
            r"\\server\share\repo\src\a.py",
            r"\\server\share",
            r"\\wsl.localhost\Ubuntu\home\u\repo\a.py",
            r"\\?\UNC\server\share\repo\a.py",
        ] {
            let resolved = resolve_cached(Path::new(root)).unwrap();
            assert!(
                resolved.to_string_lossy().starts_with(r"\\"),
                "{root}: {resolved:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let plain = fsutil::resolve_missing(dir.path()).unwrap();
        let verbatim = PathBuf::from(format!(r"\\?\{}", plain.display())).join("x.py");
        assert_eq!(resolve_cached(&verbatim).unwrap(), plain.join("x.py"));
        let slashed = format!("//?/{}/x.py", plain.to_string_lossy().replace('\\', "/"));
        assert_eq!(
            resolve_cached(Path::new(&slashed)).unwrap(),
            plain.join("x.py")
        );
    }
}
