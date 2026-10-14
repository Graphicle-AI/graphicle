// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/google_workspace.py
//! Google Drive shortcut metadata and the optional `gws` export integration.
use crate::{Result, fsutil, io_compat, process, python_compat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;

pub const GOOGLE_WORKSPACE_EXTENSIONS: &[&str] = &[".gdoc", ".gsheet", ".gslides"];
pub fn google_workspace_enabled(value: Option<&str>) -> bool {
    let environment = std::env::var("GRAPHICLE_GOOGLE_WORKSPACE").unwrap_or_default();
    matches!(
        value
            .unwrap_or(&environment)
            .trim_matches(py_space)
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
pub fn safe_yaml_str(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}
fn decoded_query(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(a), Some(b)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push((a * 16 + b) as u8);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
fn url_parts(url: &str) -> (String, String) {
    let cleaned = url
        .trim_start_matches(|c: char| c <= ' ')
        .replace(['\t', '\r', '\n'], "");
    let base = cleaned.split('#').next().unwrap_or("");
    let (head, query) = base.split_once('?').unwrap_or((base, ""));
    let after_scheme = head
        .split_once("://")
        .map(|(_, v)| v)
        .or_else(|| head.strip_prefix("//"));
    let path = after_scheme
        .map(|s| s.find('/').map(|n| &s[n..]).unwrap_or(""))
        .unwrap_or(head);
    // urlparse separates the final path component's semicolon parameters.
    let split = path.rfind('/').map_or(0, |n| n + 1);
    let path = path[split..]
        .find(';')
        .map(|n| &path[..split + n])
        .unwrap_or(path);
    (path.into(), query.into())
}
fn query_first(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|s| s.split_once('='))
        .find_map(|(key, value)| {
            let value = decoded_query(value);
            (decoded_query(key) == name && !value.is_empty()).then_some(value)
        })
}
pub fn extract_file_id_from_url(url: &str) -> Option<String> {
    let (path, query) = url_parts(url);
    if let Some(id) = query_first(&query, "id") {
        return Some(id);
    }
    let expression =
        regex::Regex::new(r"/(?:document|spreadsheets|presentation|file)/d/([^/?#]+)").unwrap();
    expression.captures(&path).map(|c| c[1].to_owned())
}
fn truth(v: &Value) -> bool {
    crate::cache_paths::truth(v)
}
pub fn extract_resource_key(url: &str, data: &Value) -> Option<String> {
    for key in ["resource_key", "resourceKey"] {
        if truth(&data[key]) {
            return Some(python_compat::string(&data[key]));
        }
    }
    query_first(&url_parts(url).1, "resourcekey")
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shortcut {
    pub file_id: String,
    pub url: Option<String>,
    pub resource_key: Option<String>,
    pub account: Option<String>,
}
pub fn read_google_shortcut(path: &Path) -> Result<Shortcut> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "could not read Google Workspace shortcut {}: {}",
            fsutil::path_string(path),
            io_compat::read_error(&e, path)
        )
    })?;
    let data: Value = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "could not read Google Workspace shortcut {}: {e}",
            fsutil::path_string(path)
        )
    })?;
    if !data.is_object() {
        return Err("Google Workspace shortcut must contain a JSON object".into());
    }
    let url = if truth(&data["url"]) {
        python_compat::string(&data["url"])
    } else {
        String::new()
    };
    let mut id = ["doc_id", "file_id", "fileId", "id"]
        .iter()
        .find_map(|k| truth(&data[k]).then(|| python_compat::string(&data[k])))
        .or_else(|| extract_file_id_from_url(&url));
    if id.as_ref().is_none_or(|s| s.is_empty()) {
        let resource = if truth(&data["resource_id"]) {
            python_compat::string(&data["resource_id"])
        } else {
            String::new()
        };
        id = resource.split_once(':').map(|(_, v)| v.to_owned());
    }
    let file_id = id.filter(|s| !s.is_empty()).ok_or_else(|| {
        format!(
            "Google Workspace shortcut {} does not include a Drive file ID",
            fsutil::path_string(path)
        )
    })?;
    Ok(Shortcut {
        file_id,
        url: (!url.is_empty()).then(|| url.clone()),
        resource_key: extract_resource_key(&url, &data),
        account: truth(&data["email"]).then(|| python_compat::string(&data["email"])),
    })
}
fn executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join(name))
        .find(|p| {
            if !p.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                p.metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
}
pub fn run_gws_export(
    file_id: &str,
    mime_type: &str,
    output: &Path,
    _resource_key: Option<&str>,
) -> Result<()> {
    let executable=executable("gws").ok_or("gws is required for Google Workspace export. Install it from https://github.com/googleworkspace/cli and run `gws auth login -s drive`.")?;
    let output = fsutil::resolve_missing(output).map_err(crate::Error::from)?;
    let parent = output
        .parent()
        .ok_or("Export output requires a parent directory")?;
    std::fs::create_dir_all(parent).map_err(crate::Error::from)?;
    let timeout = std::env::var("GRAPHICLE_GOOGLE_WORKSPACE_TIMEOUT")
        .unwrap_or_else(|_| "120".into())
        .trim()
        .parse::<i64>()
        .map_err(crate::Error::from)?;
    let params = json!({"fileId":file_id,"mimeType":mime_type});
    let result = process::capture(
        Command::new(executable)
            .args(["drive", "files", "export", "--params"])
            .arg(params.to_string())
            .arg("-o")
            .arg(output.file_name().ok_or("Missing output name")?)
            .current_dir(parent),
        Duration::from_secs(timeout.max(0) as u64),
    )
    .map_err(|e| format!("gws export failed for {file_id}: {e}"))?;
    if !result.success {
        let bytes = if result.stderr.is_empty() {
            &result.stdout
        } else {
            &result.stderr
        };
        let message = String::from_utf8_lossy(bytes);
        let message = message.trim_matches(py_space);
        let message = if message.chars().count() > 1200 {
            format!("{}...", message.chars().take(1200).collect::<String>())
        } else {
            message.into()
        };
        return Err(format!("gws export failed for {file_id}: {message}").into());
    }
    Ok(())
}
pub fn sidecar_path(path: &Path, out_dir: &Path, root: Option<&Path>) -> Result<PathBuf> {
    let fallback = out_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."));
    let absolute = fsutil::resolve_missing(path).map_err(crate::Error::from)?;
    let key = fsutil::resolve_missing(root.unwrap_or(fallback))
        .ok()
        .and_then(|root| absolute.strip_prefix(root).ok().map(fsutil::path_string))
        .unwrap_or_else(|| fsutil::path_string(&absolute));
    let digest = format!(
        "{:x}",
        Sha256::digest(key.nfc().collect::<String>().as_bytes())
    );
    Ok(out_dir.join(format!(
        "{}_{}.md",
        path.file_stem().unwrap_or_default().to_string_lossy(),
        &digest[..8]
    )))
}
pub fn with_frontmatter(path: &Path, shortcut: &Shortcut, body: &str, mime_type: &str) -> String {
    let account_line = shortcut
        .account
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|account| {
            format!(
                "google_account_hash: \"{}\"\n",
                &format!("{:x}", Sha256::digest(account.as_bytes()))[..12]
            )
        })
        .unwrap_or_default();
    format!(
        "---\nsource_file: \"{}\"\nsource_type: \"google_workspace\"\ngoogle_file_id: \"{}\"\ngoogle_export_mime_type: \"{}\"\nsource_url: \"{}\"\n{}---\n\n<!-- converted from Google Workspace shortcut: {} -->\n\n{}\n",
        safe_yaml_str(&fsutil::path_string(path)),
        safe_yaml_str(&shortcut.file_id),
        safe_yaml_str(mime_type),
        safe_yaml_str(shortcut.url.as_deref().unwrap_or("")),
        account_line,
        path.file_name().unwrap_or_default().to_string_lossy(),
        body.trim_matches(py_space)
    )
}
pub type XlsxConverter<'a> = dyn Fn(&Path) -> String + 'a;
pub type Exporter<'a> = dyn Fn(&str, &str, &Path, Option<&str>) -> Result<()> + 'a;
pub fn convert_with(
    path: &Path,
    out_dir: &Path,
    xlsx: Option<&XlsxConverter<'_>>,
    root: Option<&Path>,
    exporter: &Exporter<'_>,
) -> Result<Option<PathBuf>> {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let (mime, suffix) = match ext.as_str() {
        "gdoc" => ("text/markdown", ".md"),
        "gslides" => ("text/plain", ".txt"),
        "gsheet" => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ".xlsx",
        ),
        _ => return Ok(None),
    };
    let shortcut = read_google_shortcut(path)?;
    std::fs::create_dir_all(out_dir).map_err(crate::Error::from)?;
    let output = sidecar_path(path, out_dir, root)?;
    if ext == "gsheet" && xlsx.is_none() {
        return Err("Google Sheets export requires a native XLSX converter".into());
    }
    let temporary = tempfile::Builder::new()
        .suffix(suffix)
        .tempfile_in(out_dir)
        .map_err(crate::Error::from)?;
    exporter(
        &shortcut.file_id,
        mime,
        temporary.path(),
        shortcut.resource_key.as_deref(),
    )?;
    let body = if ext == "gsheet" {
        (xlsx.unwrap())(temporary.path())
    } else {
        fsutil::read_text_lossy(temporary.path()).map_err(crate::Error::from)?
    };
    drop(temporary);
    if body.trim_matches(py_space).is_empty() {
        return Ok(None);
    }
    std::fs::write(&output, with_frontmatter(path, &shortcut, &body, mime))
        .map_err(crate::Error::from)?;
    Ok(Some(output))
}
pub fn convert_google_workspace_file(
    path: &Path,
    out_dir: &Path,
    xlsx: Option<&XlsxConverter<'_>>,
    root: Option<&Path>,
) -> Result<Option<PathBuf>> {
    convert_with(path, out_dir, xlsx, root, &run_gws_export)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let path = Path::new(r["path"].as_str().unwrap_or(""));
    let root = r["root"].as_str().map(Path::new);
    let out = Path::new(r["out_dir"].as_str().unwrap_or(""));
    match r["method"].as_str().unwrap_or("") {
        "enabled" => Ok(json!(google_workspace_enabled(r["value"].as_str()))),
        "safe_yaml_str" => Ok(json!(safe_yaml_str(r["value"].as_str().unwrap_or("")))),
        "file_id" => Ok(json!(extract_file_id_from_url(
            r["url"].as_str().unwrap_or("")
        ))),
        "resource_key" => Ok(json!(extract_resource_key(
            r["url"].as_str().unwrap_or(""),
            &r["data"]
        ))),
        "read" => Ok(json!(read_google_shortcut(path)?)),
        "sidecar" => Ok(json!(sidecar_path(path, out, root)?)),
        "frontmatter" => Ok(json!(with_frontmatter(
            path,
            &serde_json::from_value(r["shortcut"].clone()).map_err(crate::Error::from)?,
            r["body"].as_str().unwrap_or(""),
            r["mime"].as_str().unwrap_or("")
        ))),
        "export" => {
            run_gws_export(
                r["file_id"].as_str().unwrap_or(""),
                r["mime"].as_str().unwrap_or(""),
                path,
                r["resource_key"].as_str(),
            )?;
            Ok(Value::Null)
        }
        "convert" => Ok(json!(convert_google_workspace_file(path, out, None, root)?)),
        _ => Err("Unknown Google Workspace operation".into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sheet_conversion_invokes_native_converter_and_cleans_temporary_export() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("Budget.gsheet");
        let out = temp.path().join("converted");
        std::fs::write(&input, r#"{"doc_id":"sheet-456","resource_key":"key"}"#).unwrap();
        let converter = |p: &Path| {
            assert_eq!(std::fs::read(p).unwrap(), b"xlsx");
            "## Sheet: Main\n\n| A |\n| --- |\n| 1 |".into()
        };
        let exporter = |id: &str, mime: &str, path: &Path, key: Option<&str>| {
            assert_eq!(id, "sheet-456");
            assert!(mime.ends_with(".sheet"));
            assert_eq!(key, Some("key"));
            std::fs::write(path, b"xlsx").map_err(crate::Error::from)
        };
        let result = convert_with(&input, &out, Some(&converter), None, &exporter)
            .unwrap()
            .unwrap();
        assert!(
            std::fs::read_to_string(result)
                .unwrap()
                .contains("## Sheet: Main")
        );
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        let failure = |_: &str, _: &str, _: &Path, _: Option<&str>| Err("provider failure".into());
        assert!(convert_with(&input, &out, Some(&converter), None, &failure).is_err());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
    }
}
