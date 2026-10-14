// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/pascal_forms.py
//! Format operating-system read failures like the reference's extraction errors.
use std::{io, path::Path};
pub fn read_error(error: &io::Error, path: &Path) -> String {
    let path = crate::fsutil::path_string(path);
    if path.contains('\0') {
        return "embedded null byte".into();
    }
    if let Some(code) = error.raw_os_error() {
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
