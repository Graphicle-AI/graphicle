// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
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
        crate::path_syntax::windows_posix(&path.to_string_lossy())
    }
}

/// A root-relative path as the engine reports it: `/`-separated on every
/// host (`as_posix()`); on Unix exactly `to_string_lossy`.
pub fn rel_string(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// `path` without a Win32 verbatim prefix (`\\?\C:\x` is `C:\x`), for paths
/// that come from `canonicalize` or the command line. Unchanged elsewhere.
pub fn plain(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let std::borrow::Cow::Borrowed(text) = path.to_string_lossy() else {
            return path.to_path_buf();
        };
        match crate::path_syntax::strip_verbatim(text) {
            std::borrow::Cow::Owned(stripped) => stripped.into(),
            std::borrow::Cow::Borrowed(same) if same.len() != text.len() => same.into(),
            _ => path.to_path_buf(),
        }
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// The comparison key for path identity (`os.path.normcase` after dropping
/// the verbatim prefix, as graphify's cache `_normalize_path` does): on
/// Windows `\`-separated and lower-cased, on Unix the path itself.
pub fn path_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        plain(path)
            .to_string_lossy()
            .replace('/', "\\")
            .to_lowercase()
            .into()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// `path.starts_with(root)` under [`path_key`] identity.
pub fn path_starts_with(path: &Path, root: &Path) -> bool {
    path_key(path).starts_with(path_key(root))
}

/// Path.resolve(strict=False): resolve existing symlinks while retaining missing tails.
/// On Windows the result never carries a verbatim prefix (junction targets
/// and `\\?\` arguments come back as `C:\...`), as `Path.resolve()` does.
pub fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    resolve_inner(path, &mut HashSet::new()).map(unverbatim)
}

/// Resolve `tail` starting from an already-resolved `base`. Because path
/// resolution walks components left to right, this equals resolving
/// `base_unresolved.join(tail)` in one pass, which lets callers memoise
/// directory prefixes.
pub fn resolve_missing_from(base: &Path, tail: &Path) -> io::Result<PathBuf> {
    resolve_components(base.to_path_buf(), tail, &mut HashSet::new()).map(unverbatim)
}

/// [`plain`] on Windows; elsewhere the path itself, moved.
fn unverbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        plain(&path)
    }
    #[cfg(not(windows))]
    {
        path
    }
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
    fn path_identity_is_the_path_itself_on_unix() {
        #[cfg(not(windows))]
        {
            let p = Path::new("/Repo/./src//A.py");
            assert_eq!(path_key(p).as_os_str(), p.as_os_str());
            assert_eq!(plain(p).as_os_str(), p.as_os_str());
            assert_eq!(rel_string(Path::new("src/a\\b.py")), "src/a\\b.py");
            assert!(!path_starts_with(Path::new("/repo/a"), Path::new("/Repo")));
            assert!(path_starts_with(Path::new("/repo/a"), Path::new("/repo")));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_identity_ignores_case_separators_and_verbatim_prefixes() {
        assert_eq!(
            path_string(Path::new(r"C:\Repo\src\a.py")),
            "C:/Repo/src/a.py"
        );
        assert_eq!(path_string(Path::new(r"\\?\C:\Repo\src")), "C:/Repo/src");
        assert_eq!(
            path_string(Path::new(r"\\server\share\repo")),
            "//server/share/repo"
        );
        assert_eq!(rel_string(Path::new(r"src\pkg\a.py")), "src/pkg/a.py");
        assert_eq!(plain(Path::new(r"\\?\C:\Repo")), PathBuf::from(r"C:\Repo"));
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
        assert!(path_starts_with(
            Path::new(r"\\?\C:\Repo\src\a.py"),
            Path::new("c:/repo")
        ));
        assert!(path_starts_with(
            Path::new("C:/REPO/src"),
            Path::new(r"C:\repo\")
        ));
        assert!(!path_starts_with(
            Path::new(r"C:\repo2\a"),
            Path::new(r"C:\repo")
        ));
        assert_eq!(
            path_key(Path::new("C:/Repo/A.py")),
            PathBuf::from(r"c:\repo\a.py")
        );
    }

    #[test]
    fn resolving_from_a_prefix_matches_a_full_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("pkg")).unwrap();
        std::fs::write(real.join("pkg/a.py"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        // Windows: creating a symlink needs Developer Mode or the privilege;
        // without it there is no link to resolve through.
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&real, dir.path().join("link")).is_err() {
            return;
        }
        let via_link = dir.path().join("link/pkg/../pkg/a.py");
        let full = resolve_missing(&via_link).unwrap();
        let base = resolve_missing(&dir.path().join("link/pkg/..")).unwrap();
        let stepwise = resolve_missing_from(&base, Path::new("pkg")).unwrap();
        let stepwise = resolve_missing_from(&stepwise, Path::new("a.py")).unwrap();
        assert_eq!(full, stepwise);
        assert_eq!(full, resolve_missing(&real.join("pkg/a.py")).unwrap());
    }
}
