// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
