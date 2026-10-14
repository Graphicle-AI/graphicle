// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/install.py
//! Installer file operations preserve unrelated configuration and user text.
use crate::{Result, paths};
use serde_json::{Value, json};
use std::{fs, path::Path};
pub fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(crate::Error::from)
}
pub fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(crate::Error::from)?;
    }
    paths::write_text_atomic(path, content).map_err(crate::Error::from)
}
/// `_read_keep_newline` (#4360): the file's text with bare `\n` line endings,
/// so the section helpers work on it unchanged, and the file's own dominant
/// ending (`\r\n` when it has any, else `\n`) for the write to restore.
pub fn read_keep_newline(path: &Path) -> Result<(String, &'static str)> {
    let raw = fs::read_to_string(path).map_err(crate::Error::from)?;
    let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    Ok((raw.replace("\r\n", "\n").replace('\r', "\n"), newline))
}
/// `_write_keep_newline` (#4360): write bare-`\n` text with `newline` endings.
/// A file with no convention of its own (a fresh install) is written LF.
pub fn write_keep_newline(path: &Path, text: &str, newline: &str) -> Result<()> {
    if newline == "\n" {
        write(path, text)
    } else {
        write(path, &text.replace('\n', newline))
    }
}
pub fn remove(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(crate::Error::from)?;
    Ok(true)
}
pub fn replace_section(content: &str, marker: &str, new_section: &str) -> String {
    replace_section_with_boundary(content, marker, new_section, "## ")
}
pub fn replace_section_with_boundary(
    content: &str,
    marker: &str,
    new_section: &str,
    boundary: &str,
) -> String {
    let lines = content.split('\n').collect::<Vec<_>>();
    let Some(start) = lines.iter().rposition(|l| l.trim() == marker) else {
        return if content.trim().is_empty() {
            new_section.trim_start().into()
        } else {
            format!("{}\n\n{}", content.trim_end(), new_section.trim_start())
        };
    };
    let end = (start + 1..lines.len())
        .find(|i| lines[*i].starts_with(boundary))
        .unwrap_or(lines.len());
    let head = lines[..start].join("\n");
    let tail = lines[end..].join("\n");
    let mut parts = vec![];
    if !head.trim_end().is_empty() {
        parts.push(head.trim_end());
    }
    parts.push(new_section.trim());
    if !tail.trim_start().is_empty() {
        parts.push(tail.trim_start());
    }
    let mut out = parts.join("\n\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}
pub fn remove_section(content: &str, marker: &str, boundary: &str) -> Option<String> {
    let mut content = content.to_string();
    let mut removed = false;
    loop {
        let lines = content.split('\n').collect::<Vec<_>>();
        let Some(start) = lines.iter().rposition(|s| s.trim() == marker) else {
            break;
        };
        let end = (start + 1..lines.len())
            .find(|i| lines[*i].starts_with(boundary))
            .unwrap_or(lines.len());
        let head = lines[..start].join("\n");
        let tail = lines[end..].join("\n");
        let head = head.trim_end();
        let tail = tail.trim_start();
        content = if !head.is_empty() && !tail.is_empty() {
            format!("{head}\n\n{tail}")
        } else {
            format!("{head}{tail}")
        };
        removed = true;
    }
    removed.then(|| content.trim_end().into())
}
pub fn strip_section(path: &Path, marker: &str, boundary: &str) -> Result<bool> {
    let Ok((s, newline)) = read_keep_newline(path) else {
        return Ok(false);
    };
    let Some(cleaned) = remove_section(&s, marker, boundary) else {
        return Ok(false);
    };
    // A symlinked file (e.g. CLAUDE.md -> AGENTS.md) is emptied through the
    // link rather than unlinked, so the link survives uninstall (0.9.73, #3805).
    if cleaned.is_empty() && !path.is_symlink() {
        remove(path)?;
    } else {
        // The file keeps its own line endings (#4360).
        write_keep_newline(path, &format!("{cleaned}\n"), newline)?;
    }
    Ok(true)
}
pub fn register_block(path: &Path, block: &str) -> Result<bool> {
    let (old, newline) = if path.exists() {
        read_keep_newline(path)?
    } else {
        (String::new(), "\n")
    };
    let text = replace_section_with_boundary(&old, "# graphicle", block, "# ");
    if old == text {
        return Ok(false);
    }
    write_keep_newline(path, &text, newline)?;
    Ok(true)
}
pub fn update_section(path: &Path, section: &str) -> Result<bool> {
    let (old, newline) = if path.exists() {
        read_keep_newline(path)?
    } else {
        (String::new(), "\n")
    };
    let text = if path.exists() {
        replace_section(&old, "## graphicle", section)
    } else {
        section.into()
    };
    if old == text {
        return Ok(false);
    }
    write_keep_newline(path, &text, newline)?;
    Ok(true)
}
pub fn strip_json_comments(raw: &str) -> String {
    let chars = raw.chars().collect::<Vec<_>>();
    let (mut i, mut string, mut escaped, mut line, mut block) = (0, false, false, false, false);
    let mut result = String::new();
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied().unwrap_or('\0');
        if line {
            if ch == '\n' {
                line = false;
                result.push(ch);
            }
            i += 1;
            continue;
        }
        if block {
            if ch == '*' && next == '/' {
                block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if string {
            result.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string = false;
            }
            i += 1;
            continue;
        }
        if ch == '/' && next == '/' {
            line = true;
            i += 2;
            continue;
        }
        if ch == '/' && next == '*' {
            block = true;
            i += 2;
            continue;
        }
        result.push(ch);
        if ch == '"' {
            string = true;
        }
        i += 1;
    }
    regex::Regex::new(r",(\s*[}\]])")
        .unwrap()
        .replace_all(&result, "$1")
        .into_owned()
}
pub fn json_like(path: &Path) -> Value {
    if !path.exists() {
        return json!({});
    }
    let Ok(mut raw) = read(path) else {
        return json!({});
    };
    if path.extension().is_some_and(|s| s == "jsonc") {
        raw = strip_json_comments(&raw);
    }
    serde_json::from_str::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or(json!({}))
}
pub fn refuse(path: &Path) -> String {
    format!(
        "[graphicle] refusing to modify {}: not valid JSON (fix or move it and re-run)",
        path.display()
    )
}
pub fn settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(read(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')).ok())
        .filter(Value::is_object)
        .ok_or_else(|| refuse(path))?)
}
pub fn json_text(v: &Value) -> String {
    let mut b = vec![];
    crate::python_json::write(&mut b, v, Some(2), true).expect("writing JSON to memory");
    String::from_utf8(b).unwrap()
}
pub fn write_settings(path: &Path, value: &Value, backup: bool) -> Result<bool> {
    let text = json_text(value);
    if path.exists() {
        if read(path)? == text {
            return Ok(false);
        }
        if backup {
            fs::copy(
                path,
                path.with_file_name(format!(
                    "{}.graphicle-bak",
                    path.file_name().unwrap().to_string_lossy()
                )),
            )
            .map_err(crate::Error::from)?;
        }
    }
    write(path, &text)?;
    Ok(true)
}
pub fn install_refs(dst: &Path, source: &Path) -> Result<()> {
    fn copy(src: &Path, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst).map_err(crate::Error::from)?;
        for e in fs::read_dir(src).map_err(crate::Error::from)? {
            let e = e.map_err(crate::Error::from)?;
            let out = dst.join(e.file_name());
            if e.path().is_dir() {
                copy(&e.path(), &out)?;
            } else {
                fs::copy(e.path(), &out).map_err(crate::Error::from)?;
                let mut permissions = out.metadata().map_err(crate::Error::from)?.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(permissions.mode() | 0o200);
                }
                #[cfg(not(unix))]
                permissions.set_readonly(false);
                fs::set_permissions(out, permissions).map_err(crate::Error::from)?;
            }
        }
        Ok(())
    }
    let dir = dst.parent().ok_or("Skill path needs a parent")?;
    let staged = dir.join("references.tmp");
    let final_path = dir.join("references");
    remove(&staged)?;
    if let Err(e) = copy(source, &staged) {
        let _ = remove(&staged);
        return Err(e);
    }
    remove(&final_path)?;
    fs::rename(staged, final_path).map_err(crate::Error::from)
}
