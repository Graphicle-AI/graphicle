//! Codex's MCP servers in its `config.toml` (`[mcp_servers.<name>]` tables),
//! read and changed as text: the runtime has no TOML dependency, and these
//! changes touch only the named tables, so every other line of the file
//! stays as it was.
//!
//! `graphicle connect codex` adds its entry through the Codex CLI when it is
//! installed (`codex mcp add`); without the CLI (the Codex IDE extension
//! keeps its own copy of it) the table is written here, between marker
//! comments. Either way `graphicle disconnect` removes the table here.
use std::path::{Path, PathBuf};

/// The line before a table Graphicle wrote.
pub const START: &str =
    "# graphicle-start (added by graphicle connect; graphicle disconnect removes it)";
/// The line after it.
pub const END: &str = "# graphicle-end";

/// Codex's configuration file: `$CODEX_HOME/config.toml`, else
/// `~/.codex/config.toml`.
pub fn config_path(codex_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    codex_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".codex")))
        .map(|folder| folder.join("config.toml"))
}

/// One `[mcp_servers.<name>]` table: its lines `start..end` (its header to
/// its last value line, its subtables included), and the server it names.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
}

impl Table {
    /// The entry as a JSON MCP entry (`command`, `args`), to compare.
    pub fn entry(&self) -> serde_json::Value {
        serde_json::json!({"command": self.command, "args": self.args})
    }
}

/// How a line of the file reads, as far as tables go.
#[derive(Debug, PartialEq)]
enum Line {
    /// `[a.b]` or `[[a.b]]`: the dotted key's parts.
    Header(Vec<String>),
    /// A key/value line, or a line continuing a multi-line value.
    Content,
    /// A blank or comment line outside any value.
    Blank,
}

/// Classify every line of `text`, following multi-line strings and arrays
/// so that a line inside a value is never taken for a header.
fn classify(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    // Open multi-line string delimiter, and the depth of open arrays and
    // inline tables, carried from line to line.
    let mut open: Option<&'static str> = None;
    let mut depth = 0usize;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if open.is_none() && depth == 0 {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                lines.push(Line::Blank);
                continue;
            }
            if trimmed.starts_with('[') {
                lines.push(match header(trimmed) {
                    Some(parts) => Line::Header(parts),
                    None => Line::Content,
                });
                continue;
            }
        }
        lines.push(Line::Content);
        scan(line, &mut open, &mut depth);
    }
    lines
}

/// Follow strings, comments and brackets through one line.
fn scan(line: &str, open: &mut Option<&'static str>, depth: &mut usize) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(delimiter) = *open {
            if line[i..].starts_with(delimiter) {
                *open = None;
                i += 3;
            } else {
                if delimiter == "\"\"\"" && bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        match bytes[i] {
            b'#' => return,
            b'"' if line[i..].starts_with("\"\"\"") => {
                *open = Some("\"\"\"");
                i += 3;
            }
            b'\'' if line[i..].starts_with("'''") => {
                *open = Some("'''");
                i += 3;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'[' | b'{' => {
                *depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                *depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
}

/// The dotted key of a table header line, `None` when it is not one.
fn header(trimmed: &str) -> Option<Vec<String>> {
    let (inner, rest) = if let Some(rest) = trimmed.strip_prefix("[[") {
        let end = rest.find("]]")?;
        (&rest[..end], &rest[end + 2..])
    } else {
        let rest = trimmed.strip_prefix('[')?;
        let end = rest.find(']')?;
        (&rest[..end], &rest[end + 1..])
    };
    let rest = rest.trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return None;
    }
    let mut parts = Vec::new();
    let mut remaining = inner.trim();
    loop {
        let (part, after) = key(remaining)?;
        parts.push(part);
        let after = after.trim_start();
        if after.is_empty() {
            break;
        }
        remaining = after.strip_prefix('.')?.trim_start();
    }
    Some(parts)
}

/// One key (bare, "basic" or 'literal') at the start of `text`, and the rest.
fn key(text: &str) -> Option<(String, &str)> {
    if text.starts_with('"') || text.starts_with('\'') {
        return string(text);
    }
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(text.len());
    (end > 0).then(|| (text[..end].to_string(), &text[end..]))
}

/// One TOML string (basic or literal, on one line) at the start of `text`,
/// and the rest.
fn string(text: &str) -> Option<(String, &str)> {
    if let Some(rest) = text.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return Some((rest[..end].to_string(), &rest[end + 1..]));
    }
    let rest = text.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((value, &rest[i + 1..])),
            '\\' => {
                let (_, escape) = chars.next()?;
                match escape {
                    'b' => value.push('\u{8}'),
                    't' => value.push('\t'),
                    'n' => value.push('\n'),
                    'f' => value.push('\u{c}'),
                    'r' => value.push('\r'),
                    'e' => value.push('\u{1b}'),
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'u' | 'U' => {
                        let width = if escape == 'u' { 4 } else { 8 };
                        let mut code = String::new();
                        for _ in 0..width {
                            code.push(chars.next()?.1);
                        }
                        value.push(char::from_u32(u32::from_str_radix(&code, 16).ok()?)?);
                    }
                    _ => return None,
                }
            }
            c => value.push(c),
        }
    }
    None
}

/// An array of one-line strings, `["a", 'b']`, possibly over several lines.
fn string_array(text: &str) -> Option<Vec<String>> {
    let mut rest = text.trim_start().strip_prefix('[')?;
    let mut values = Vec::new();
    loop {
        rest = skip_space(rest);
        if let Some(after) = rest.strip_prefix(']') {
            return skip_space(after).is_empty().then_some(values);
        }
        let (value, after) = string(rest)?;
        values.push(value);
        rest = skip_space(after);
        if let Some(after) = rest.strip_prefix(',') {
            rest = after;
        } else if !rest.starts_with(']') {
            return None;
        }
    }
}

/// `text` from its first character that is not white space or in a comment.
fn skip_space(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        match text.strip_prefix('#') {
            Some(comment) => text = comment.split_once('\n').map_or("", |(_, rest)| rest),
            None => return text,
        }
    }
}

/// The `[mcp_servers.<name>]` tables of `text`, in order.
pub fn tables(text: &str) -> Vec<Table> {
    let lines: Vec<&str> = text.split('\n').collect();
    let kinds = classify(text);
    let mut found: Vec<Table> = Vec::new();
    let mut i = 0;
    while i < kinds.len() {
        let Line::Header(parts) = &kinds[i] else {
            i += 1;
            continue;
        };
        if parts.len() != 2 || parts[0] != "mcp_servers" {
            i += 1;
            continue;
        }
        let name = parts[1].clone();
        let start = i;
        let mut end = i + 1;
        let mut j = i + 1;
        while j < kinds.len() {
            match &kinds[j] {
                Line::Header(other)
                    if !(other.len() > 2 && other[0] == "mcp_servers" && other[1] == name) =>
                {
                    break;
                }
                Line::Blank => {}
                _ => end = j + 1,
            }
            j += 1;
        }
        let (command, args) = server(&lines[start + 1..end], &kinds[start + 1..end]);
        found.push(Table {
            name,
            start,
            end,
            command,
            args,
        });
        i = j;
    }
    found
}

/// `command` and `args` of a table's own lines (before any subtable).
fn server(lines: &[&str], kinds: &[Line]) -> (Option<String>, Option<Vec<String>>) {
    let (mut command, mut args) = (None, None);
    let mut i = 0;
    while i < lines.len() {
        if matches!(kinds[i], Line::Header(_)) {
            break;
        }
        let Some((name, value)) = lines[i].split_once('=') else {
            i += 1;
            continue;
        };
        let name = key(name.trim()).map(|(name, _)| name);
        // A value that goes on over the next lines, up to the next key,
        // header or the table's end.
        let mut text = value.to_string();
        let mut next = i + 1;
        while next < lines.len() && matches!(kinds[next], Line::Content) && !starts_key(lines[next])
        {
            text.push('\n');
            text.push_str(lines[next]);
            next += 1;
        }
        match name.as_deref() {
            Some("command") => {
                command = string(text.trim())
                    .and_then(|(value, rest)| skip_space(rest).is_empty().then_some(value));
            }
            Some("args") => args = string_array(&text),
            _ => {}
        }
        i = next;
    }
    (command, args)
}

/// Whether `line` starts a new `key = value` (rather than continuing a
/// multi-line value).
fn starts_key(line: &str) -> bool {
    let trimmed = line.trim_start();
    key(trimmed).is_some_and(|(_, rest)| rest.trim_start().starts_with('='))
}

/// Whether `text` defines `mcp_servers` otherwise than as `[mcp_servers.x]`
/// tables (an inline table or dotted keys at the top level), which a new
/// table would conflict with.
pub fn inline_servers(text: &str) -> bool {
    let kinds = classify(text);
    let mut top = true;
    for (line, kind) in text.split('\n').zip(&kinds) {
        match kind {
            Line::Header(parts) => {
                top = false;
                if parts.len() == 1 && parts[0] == "mcp_servers" {
                    return true;
                }
            }
            Line::Content
                if top && key(line.trim_start()).is_some_and(|(name, _)| name == "mcp_servers") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// A TOML basic string holding `value`.
fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A table key: bare when it can be.
fn table_key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        name.to_string()
    } else {
        quoted(name)
    }
}

/// The marked table Graphicle writes for an entry.
pub fn block(name: &str, command: &str, args: &[String]) -> String {
    let args: Vec<String> = args.iter().map(|arg| quoted(arg)).collect();
    format!(
        "{START}\n[mcp_servers.{}]\ncommand = {}\nargs = [{}]\n{END}\n",
        table_key(name),
        quoted(command),
        args.join(", ")
    )
}

/// `text` with the marked table for `name` added at its end, after a blank
/// line.
pub fn append(text: &str, name: &str, command: &str, args: &[String]) -> String {
    let mut out = text.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&block(name, command, args));
    out
}

/// `text` without the table `name` (its subtables and Graphicle's marker
/// lines around it included), the removed lines and the line they started
/// at, so that [`reinsert`] puts them back exactly; `None` when there is no
/// such table.
pub fn remove(text: &str, name: &str) -> Option<(String, String, usize)> {
    let table = tables(text).into_iter().find(|table| table.name == name)?;
    let mut lines: Vec<&str> = text.split('\n').collect();
    let mut start = table.start;
    let mut end = table.end;
    if start > 0
        && lines[start - 1]
            .trim_start()
            .starts_with("# graphicle-start")
    {
        start -= 1;
    }
    if end < lines.len() && lines[end].trim() == END {
        end += 1;
    }
    // The blank line before the table goes with it when, without the
    // table, it would double another blank line or end the file.
    if start > 0
        && lines[start - 1].trim().is_empty()
        && (end >= lines.len() || lines[end].trim().is_empty())
    {
        start -= 1;
    }
    let removed = lines[start..end].join("\n");
    lines.drain(start..end);
    Some((lines.join("\n"), removed, start))
}

/// `text` with `removed` (lines [`remove`] returned) put back at line `at`,
/// or before the file's final newline when it is shorter now.
pub fn reinsert(text: &str, removed: &str, at: usize) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A file ending in a newline splits into a last empty line, which stays
    // last.
    let last = if lines.len() > 1 && lines.last() == Some(&"") {
        lines.len() - 1
    } else {
        lines.len()
    };
    let at = at.min(last);
    let mut out: Vec<&str> = lines.drain(..at).collect();
    out.extend(removed.split('\n'));
    out.extend(lines);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"model = "o4"

# The docs server.
[mcp_servers.docs]
command = "npx"
args = [
  "-y", # the package
  "docs-mcp",
]

[mcp_servers.docs.env]
TOKEN_NAME = "DOCS"

[profiles.fast]
model = "o4-mini"
"#;

    #[test]
    fn tables_follow_multi_line_values_and_subtables() {
        let found = tables(CONFIG);
        assert_eq!(found.len(), 1);
        let docs = &found[0];
        assert_eq!(docs.name, "docs");
        assert_eq!(docs.command.as_deref(), Some("npx"));
        assert_eq!(
            docs.args.as_deref(),
            Some(&["-y".to_string(), "docs-mcp".to_string()][..])
        );
        let lines: Vec<&str> = CONFIG.split('\n').collect();
        assert_eq!(lines[docs.start], "[mcp_servers.docs]");
        assert_eq!(lines[docs.end - 1], "TOKEN_NAME = \"DOCS\"");
        assert!(!inline_servers(CONFIG));
        assert!(inline_servers(
            "mcp_servers = { a = { command = \"x\" } }\n"
        ));
        assert!(inline_servers("[mcp_servers]\nx.command = \"y\"\n"));
    }

    #[test]
    fn a_written_table_reads_back_and_its_removal_restores_the_file() {
        let args: Vec<String> = [
            "--data-dir",
            "/home/a b/data \"x\"",
            "mcp",
            "--project",
            "p1",
        ]
        .map(String::from)
        .to_vec();
        let written = append(CONFIG, "graphicle-app", "/opt/bin/graphicle", &args);
        let table = tables(&written)
            .into_iter()
            .find(|table| table.name == "graphicle-app")
            .unwrap();
        assert_eq!(table.command.as_deref(), Some("/opt/bin/graphicle"));
        assert_eq!(table.args.as_deref(), Some(&args[..]));
        let (back, removed, at) = remove(&written, "graphicle-app").unwrap();
        assert_eq!(back, CONFIG);
        assert!(
            removed.contains(START) && removed.ends_with(END),
            "{removed}"
        );
        assert_eq!(reinsert(&back, &removed, at), written);
        // Codex's own format (`codex mcp add`), without markers.
        let codex =
            format!("{CONFIG}\n[mcp_servers.graphicle-app]\ncommand = \"g\"\nargs = [\"mcp\"]\n");
        let (back, _, _) = remove(&codex, "graphicle-app").unwrap();
        assert_eq!(back, CONFIG);
        assert!(remove(CONFIG, "graphicle-app").is_none());
    }

    #[test]
    fn removing_a_table_keeps_the_comments_of_the_next() {
        let text = "[mcp_servers.graphify]\ncommand = \"graphify\"\nargs = [\"--mcp\"]\n\n# Mine.\n[mcp_servers.mine]\ncommand = \"m\"\n";
        let (back, removed, at) = remove(text, "graphify").unwrap();
        assert_eq!(back, "\n# Mine.\n[mcp_servers.mine]\ncommand = \"m\"\n");
        assert_eq!(reinsert(&back, &removed, at), text);
    }
}
