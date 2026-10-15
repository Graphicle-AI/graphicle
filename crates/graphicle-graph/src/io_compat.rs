// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/pascal_forms.py
//! Format operating-system read failures like the reference's extraction errors.
use std::{io, path::Path};
pub fn read_error(error: &io::Error, path: &Path) -> String {
    #[cfg(windows)]
    let target = path;
    let path = crate::fsutil::path_string(path);
    if path.contains('\0') {
        return "embedded null byte".into();
    }
    if let Some(code) = error.raw_os_error() {
        #[cfg(windows)]
        if let Some((errno, message)) = python_errno(code, target) {
            return format!(
                "[Errno {errno}] {message}: {}",
                crate::python_compat::repr_string(&path)
            );
        }
        let message = error.to_string();
        let suffix = format!(" (os error {code})");
        let message = message.strip_suffix(&suffix).unwrap_or(&message);
        format!(
            "[Errno {code}] {message}: {}",
            crate::python_compat::repr_string(&path)
        )
    } else {
        error.to_string()
    }
}
/// Windows error codes as the reference's Unix wording names them: missing
/// file or folder (2, 3) is ENOENT, access denied (5) is EACCES, or EISDIR
/// when the path is a folder (Windows refuses to open one for reading).
#[cfg(windows)]
fn python_errno(code: i32, path: &Path) -> Option<(i32, &'static str)> {
    match code {
        2 | 3 => Some((2, "No such file or directory")),
        5 if path.is_dir() => Some((21, "Is a directory")),
        5 => Some((13, "Permission denied")),
        _ => None,
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::read_error;
    use std::{io, path::Path};

    #[test]
    fn windows_codes_use_python_wording() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").join("a.py");
        let error = io::Error::from_raw_os_error(3);
        let missing_text = "[Errno 2] No such file or directory: ";
        assert!(read_error(&error, &missing).starts_with(missing_text));
        let error = io::Error::from_raw_os_error(2);
        assert!(read_error(&error, &missing).starts_with(missing_text));
        let error = std::fs::read(dir.path()).unwrap_err();
        assert!(read_error(&error, dir.path()).starts_with("[Errno 21] Is a directory: "));
        let error = io::Error::from_raw_os_error(5);
        let denied = read_error(&error, Path::new(r"C:\no\such\file"));
        assert!(denied.starts_with("[Errno 13] Permission denied: "));
    }
}
