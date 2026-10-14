// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/sln.py
//! Native filesystem helpers for the translated application modules.
use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};

pub fn read_text_lossy(path: &Path) -> io::Result<String> {
    crate::extractors::parse_cache::read(path).map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    })
}

pub fn path_string(path: &Path) -> String {
    #[cfg(not(windows))]
    {
        crate::path_syntax::posix_join(&crate::path_syntax::posix_parts(&path.to_string_lossy()))
    }
    #[cfg(windows)]
    {
        path.to_string_lossy().into_owned()
    }
}

/// Path.resolve(strict=False): resolve existing symlinks while retaining missing tails.
pub fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    resolve_inner(path, &mut HashSet::new())
}

/// Resolve `tail` starting from an already-resolved `base`. Because path
/// resolution walks components left to right, this equals resolving
/// `base_unresolved.join(tail)` in one pass, which lets callers memoise
/// directory prefixes.
pub fn resolve_missing_from(base: &Path, tail: &Path) -> io::Result<PathBuf> {
    resolve_components(base.to_path_buf(), tail, &mut HashSet::new())
}

fn resolve_inner(path: &Path, active: &mut HashSet<PathBuf>) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    resolve_components(PathBuf::new(), &absolute, active)
}

fn resolve_components(
    mut result: PathBuf,
    path: &Path,
    active: &mut HashSet<PathBuf>,
) -> io::Result<PathBuf> {
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(name) => {
                result.push(name);
                if std::fs::symlink_metadata(&result).is_ok_and(|m| m.file_type().is_symlink()) {
                    let link = result.clone();
                    if active.len() >= 64 || !active.insert(link.clone()) {
                        return Err(io::Error::other("Symlink loop"));
                    }
                    let target = std::fs::read_link(&result)?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        result.parent().unwrap_or(Path::new(".")).join(target)
                    };
                    result = resolve_inner(&target, active)?;
                    active.remove(&link);
                }
            }
        }
    }
    Ok(result)
}

pub fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut remaining = text;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        if let Some((offset, separator)) = remaining.char_indices().find(|(_, c)| {
            matches!(
                c,
                '\n' | '\r'
                    | '\x0b'
                    | '\x0c'
                    | '\x1c'
                    | '\x1d'
                    | '\x1e'
                    | '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
        }) {
            let line = &remaining[..offset];
            let mut next = offset + separator.len_utf8();
            if separator == '\r' && remaining.as_bytes().get(next) == Some(&b'\n') {
                next += 1;
            }
            remaining = &remaining[next..];
            Some(line)
        } else {
            let line = remaining;
            remaining = "";
            Some(line)
        }
    })
}

pub const PY_WHITESPACE: &str = r"[\t\n\r\x0B\x0C\x1C-\x1F\x20\x85\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}]";

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn resolving_from_a_prefix_matches_a_full_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("pkg")).unwrap();
        std::fs::write(real.join("pkg/a.py"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let via_link = dir.path().join("link/pkg/../pkg/a.py");
        let full = resolve_missing(&via_link).unwrap();
        let base = resolve_missing(&dir.path().join("link/pkg/..")).unwrap();
        let stepwise = resolve_missing_from(&base, Path::new("pkg")).unwrap();
        let stepwise = resolve_missing_from(&stepwise, Path::new("a.py")).unwrap();
        assert_eq!(full, stepwise);
        assert_eq!(full, resolve_missing(&real.join("pkg/a.py")).unwrap());
    }
}
