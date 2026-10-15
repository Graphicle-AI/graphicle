// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/base.py
//! Portable lexical helpers. These do not canonicalize or read the filesystem.
pub fn posix_parts(path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    if path.starts_with("//") && !path.starts_with("///") {
        parts.push("//".into());
    } else if path.starts_with('/') {
        parts.push("/".into());
    }
    parts.extend(
        path.split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned),
    );
    parts
}

pub fn posix_join(parts: &[String]) -> String {
    if parts.is_empty() {
        return ".".into();
    }
    if parts[0] == "/" || parts[0] == "//" {
        format!("{}{}", parts[0], parts[1..].join("/"))
    } else {
        parts.join("/")
    }
}

pub fn posix_name(parts: &[String]) -> &str {
    parts
        .last()
        .filter(|part| *part != "/" && *part != "//")
        .map(String::as_str)
        .unwrap_or("")
}

pub fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => &name[index..],
        _ => "",
    }
}

/// Drop a Win32 verbatim prefix where a plain spelling names the same file:
/// `\\?\C:\x` is `C:\x` and `\\?\UNC\server\share\x` is `\\server\share\x`.
/// Also takes the forward-slashed `//?/` form that older Windows caches
/// stored. Other verbatim paths (volume GUIDs, devices) are returned as they
/// are. Windows only: elsewhere these strings are ordinary names.
#[cfg(any(windows, test))]
pub fn strip_verbatim(path: &str) -> std::borrow::Cow<'_, str> {
    let Some(rest) = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix("//?/"))
    else {
        return path.into();
    };
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return rest.into();
    }
    if bytes.len() >= 4
        && bytes[..3].eq_ignore_ascii_case(b"UNC")
        && matches!(bytes[3], b'\\' | b'/')
    {
        return format!(r"\\{}", &rest[4..]).into();
    }
    path.into()
}

/// The engine's one spelling of a Windows path: `/`-separated, without a
/// verbatim prefix, the drive or UNC share kept (`C:/repo/src/a.py`,
/// `//server/share/repo`), empty and `.` components dropped as
/// `PureWindowsPath(...).as_posix()` does.
#[cfg(any(windows, test))]
pub fn windows_posix(path: &str) -> String {
    let path = strip_verbatim(path).replace('/', "\\");
    let (drive, root, rest) = crate::detect_manifest::ntpath_splitroot(&path);
    let rest = rest
        .split('\\')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    let spelled = format!("{drive}{root}").replace('\\', "/") + &rest;
    if spelled.is_empty() {
        ".".into()
    } else {
        spelled
    }
}

#[cfg(test)]
mod windows_spelling_tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_stripped_only_where_a_plain_spelling_exists() {
        assert_eq!(strip_verbatim(r"\\?\C:\repo\a.py"), r"C:\repo\a.py");
        assert_eq!(strip_verbatim("//?/C:/repo/a.py"), "C:/repo/a.py");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\repo"),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_verbatim(r"\\?\unc\wsl.localhost\Ubuntu"),
            r"\\wsl.localhost\Ubuntu"
        );
        assert_eq!(strip_verbatim(r"\\?\Volume{1}\x"), r"\\?\Volume{1}\x");
        assert_eq!(strip_verbatim(r"\\server\share\x"), r"\\server\share\x");
        assert_eq!(strip_verbatim("/home/u/repo"), "/home/u/repo");
    }

    #[test]
    fn windows_paths_have_one_forward_slash_spelling() {
        assert_eq!(windows_posix(r"C:\repo\src\a.py"), "C:/repo/src/a.py");
        assert_eq!(windows_posix(r"C:\repo\src/a.py"), "C:/repo/src/a.py");
        assert_eq!(
            windows_posix(r"\\?\C:\repo\.\src\\a.py"),
            "C:/repo/src/a.py"
        );
        assert_eq!(
            windows_posix(r"\\?\UNC\server\share\repo\"),
            "//server/share/repo"
        );
        assert_eq!(windows_posix(r"\\server\share\repo"), "//server/share/repo");
        assert_eq!(
            windows_posix(r"\\wsl.localhost\Ubuntu\home\u\repo"),
            "//wsl.localhost/Ubuntu/home/u/repo"
        );
        assert_eq!(windows_posix(r"C:\"), "C:/");
        assert_eq!(windows_posix("C:"), "C:");
        assert_eq!(windows_posix(r"src\a.py"), "src/a.py");
        assert_eq!(windows_posix(""), ".");
    }
}
