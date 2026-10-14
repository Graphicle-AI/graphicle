//! Small edits of the JSON files agent clients keep (`.mcp.json`,
//! `.vscode/mcp.json`, Cursor's and Claude Desktop's files), made in the
//! text itself so that everything else stays as it was: the indentation,
//! compact arrays, comments, the final newline.
//!
//! VS Code's `mcp.json` is JSONC: comments and trailing commas are allowed.
//! [`parse`] reads both forms. [`set`] and [`remove`] change one entry of
//! a top-level object (`mcpServers` or `servers`): an entry on lines of its
//! own is added as lines after the last entry's line (a comment on that
//! line stays there) and removed with its lines (comments on other lines
//! stay), so removing what was added gives the text back byte for byte. A
//! caller checks the result with [`parse`] and otherwise writes [`render`]'s
//! text, which keeps the file's indentation and final newline but not its
//! comments (so a file with comments is never rewritten that way).
use serde_json::Value;

/// The value of a JSON or JSONC text: comments (`//`, `/* */`) and trailing
/// commas are allowed. `None` when it is neither.
pub fn parse(text: &str) -> Option<Value> {
    serde_json::from_str(text)
        .ok()
        .or_else(|| serde_json::from_str(&strip(text)).ok())
}

/// Where a JSON or JSONC text stops being valid, for an error message:
/// `line L, column C`.
pub fn error_place(text: &str) -> String {
    match serde_json::from_str::<Value>(&strip(text)) {
        Err(error) => format!("line {}, column {}", error.line(), error.column()),
        Ok(_) => "unknown place".into(),
    }
}

/// Whether the text holds a comment.
pub fn has_comments(text: &str) -> bool {
    strip(text) != text && {
        // Trailing commas alone are not comments.
        let bytes = text.as_bytes();
        let mut at = 0;
        let mut found = false;
        while at < bytes.len() {
            match bytes[at] {
                b'"' => at = string_end(bytes, at).unwrap_or(bytes.len()),
                b'/' if matches!(bytes.get(at + 1), Some(b'/' | b'*')) => {
                    found = true;
                    break;
                }
                _ => at += 1,
            }
        }
        found
    }
}

/// The comments of a JSONC text, in order (`// …` without its line end,
/// `/* … */` whole).
pub fn comments(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'"' => at = string_end(bytes, at).unwrap_or(bytes.len()),
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let end = text[at..].find('\n').map_or(bytes.len(), |end| at + end);
                found.push(&text[at..end]);
                at = end;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let end = text[at + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| at + 2 + end + 2);
                found.push(&text[at..end]);
                at = end;
            }
            _ => at += 1,
        }
    }
    found
}

/// `text` with its comments and trailing commas replaced by spaces, so that
/// every other byte keeps its offset (line breaks inside comments stay).
pub fn strip(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'"' => at = string_end(bytes, at).unwrap_or(bytes.len()),
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    out[at] = b' ';
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let end = text[at + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| at + 2 + end + 2);
                for byte in &mut out[at..end] {
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                }
                at = end;
            }
            b',' => {
                // A comma followed (past blanks and comments) by `}` or `]`.
                let next = skip(bytes, at + 1);
                if matches!(bytes.get(next), Some(b'}' | b']')) {
                    out[at] = b' ';
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    // Only ASCII bytes were replaced, by ASCII spaces.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// The offset after the string that starts at `at` (a `"`).
fn string_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The offset of the next byte that is neither blank nor in a comment.
fn skip(bytes: &[u8], mut at: usize) -> usize {
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) == Some(&b'/') && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if bytes.get(at) == Some(&b'/') && bytes.get(at + 1) == Some(&b'*') {
            let rest = &bytes[at + 2..];
            at = rest
                .windows(2)
                .position(|pair| pair == b"*/")
                .map_or(bytes.len(), |end| at + 2 + end + 2);
        } else {
            return at;
        }
    }
}

/// What follows the value that ends at `at` on its line, when that is only
/// blanks, at most one comma and comments ending on the line: the comma's
/// offset, and the offset of the line's end (its `\n`, or the text's end).
/// `None` when anything else follows on the line (another member, a
/// closing bracket) or a comment runs on to another line.
fn rest_of_line(bytes: &[u8], mut at: usize) -> Option<(Option<usize>, usize)> {
    let mut comma = None;
    loop {
        match bytes.get(at) {
            None | Some(b'\n') => return Some((comma, at)),
            Some(b' ' | b'\t' | b'\r') => at += 1,
            Some(b',') if comma.is_none() => {
                comma = Some(at);
                at += 1;
            }
            Some(b'/') if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            Some(b'/') if bytes.get(at + 1) == Some(&b'*') => {
                let end = bytes[at + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map(|end| at + 2 + end + 2)?;
                if bytes[at..end].contains(&b'\n') {
                    return None;
                }
                at = end;
            }
            Some(_) => return None,
        }
    }
}

/// The start of the line that `at` is on.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The offset after the value that starts at `at`.
fn value_end(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at)? {
        b'"' => string_end(bytes, at),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = at;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        i = string_end(bytes, i)?;
                        continue;
                    }
                    b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => {
                        i = skip(bytes, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let mut i = at;
            while i < bytes.len() && !b",}] \t\r\n/".contains(&bytes[i]) {
                i += 1;
            }
            (i > at).then_some(i)
        }
    }
}

/// A member of an object in the text.
#[derive(Debug)]
struct Member {
    key: String,
    /// Where its key starts.
    start: usize,
    /// Where its value starts and ends.
    value: (usize, usize),
}

/// An object in the text: its braces and members.
#[derive(Debug)]
struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

/// The object whose `{` is at `open`.
fn object(text: &str, open: usize) -> Option<Object> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut members = Vec::new();
    let mut at = skip(bytes, open + 1);
    loop {
        match bytes.get(at)? {
            b'}' => {
                return Some(Object {
                    open,
                    close: at,
                    members,
                });
            }
            b'"' => {
                let end = string_end(bytes, at)?;
                let key: String = serde_json::from_str(&text[at..end]).ok()?;
                let colon = skip(bytes, end);
                if bytes.get(colon) != Some(&b':') {
                    return None;
                }
                let value = skip(bytes, colon + 1);
                let value_end = value_end(bytes, value)?;
                members.push(Member {
                    key,
                    start: at,
                    value: (value, value_end),
                });
                at = skip(bytes, value_end);
                match bytes.get(at)? {
                    b',' => at = skip(bytes, at + 1),
                    b'}' => {}
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
}

/// The root object of a text.
fn root(text: &str) -> Option<Object> {
    object(text, skip(text.as_bytes(), 0))
}

/// The blanks that start the line of `at`, when only blanks and comments
/// precede `at` on that line (`None`: something else is on the line before
/// it, as in a compact object).
fn line_indent(text: &str, at: usize) -> Option<&str> {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..at];
    if !strip(before).chars().all(|c| c == ' ' || c == '\t') {
        return None;
    }
    let blanks = before.len() - before.trim_start_matches([' ', '\t']).len();
    Some(&before[..blanks])
}

/// The indentation unit of a text: the blanks before the root's first
/// member, else two spaces.
fn unit(text: &str) -> String {
    root(text)
        .and_then(|object| {
            let first = object.members.first()?;
            line_indent(text, first.start)
                .filter(|indent| !indent.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// `value` as JSON at the indentation `indent` (the indentation of its
/// member's line), each level one `unit` deeper; on one line when `compact`.
fn written(value: &Value, unit: &str, indent: &str, compact: bool) -> String {
    if compact {
        return value.to_string();
    }
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(unit.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    if serde::Serialize::serialize(value, &mut serializer).is_err() {
        return value.to_string();
    }
    String::from_utf8(out)
        .unwrap_or_default()
        .replace('\n', &format!("\n{indent}"))
}

/// Insert member `name: value` at the end of `object` (in `text`), styled
/// as the object's other members, or one level deeper than `parent_indent`
/// in an empty object.
fn insert(text: &str, object: &Object, name: &str, value: &Value, parent_indent: &str) -> String {
    let unit = unit(text);
    let key = Value::String(name.to_string()).to_string();
    match object.members.last() {
        Some(last) => {
            let first = &object.members[0];
            // Members on lines of their own: the new one goes on the lines
            // after the last member's line, so a comment at the end of that
            // line stays with it; a trailing comma there gets one here too.
            if let (Some(indent), Some(_), Some((comma, end))) = (
                line_indent(text, first.start),
                line_indent(text, last.start),
                rest_of_line(text.as_bytes(), last.value.1),
            ) && end < text.len()
            {
                let entry = format!(
                    "{indent}{key}: {}{}\n",
                    written(value, &unit, indent, false),
                    if comma.is_some() { "," } else { "" }
                );
                return match comma {
                    Some(_) => format!("{}{entry}{}", &text[..=end], &text[end + 1..]),
                    None => format!(
                        "{},{}{entry}{}",
                        &text[..last.value.1],
                        &text[last.value.1..=end],
                        &text[end + 1..]
                    ),
                };
            }
            let compact = line_indent(text, first.start).is_none();
            let (indent, separator) = if compact {
                (String::new(), String::new())
            } else {
                let indent = line_indent(text, first.start)
                    .unwrap_or_default()
                    .to_string();
                (indent.clone(), format!("\n{indent}"))
            };
            let colon = if compact { ":" } else { ": " };
            format!(
                "{},{separator}{key}{colon}{}{}",
                &text[..last.value.1],
                written(value, &unit, &indent, compact),
                &text[last.value.1..]
            )
        }
        None => {
            let inner = format!("{parent_indent}{unit}");
            // An empty object is filled on one line only in a file whose
            // root object, with members, is on one line.
            let compact = root(text).is_some_and(|root| {
                !root.members.is_empty() && !text[root.open..root.close].contains('\n')
            });
            if compact {
                format!(
                    "{}{{{key}:{}}}{}",
                    &text[..object.open],
                    value,
                    &text[object.close + 1..]
                )
            } else {
                format!(
                    "{}{{\n{inner}{key}: {}\n{parent_indent}}}{}",
                    &text[..object.open],
                    written(value, &unit, &inner, false),
                    &text[object.close + 1..]
                )
            }
        }
    }
}

/// `text` with `text[group][name]` set to `value`, every other byte kept;
/// `group` is a member of the root object, added when missing. `None` when
/// the text cannot be edited in place.
pub fn set(text: &str, group: &str, name: &str, value: &Value) -> Option<String> {
    let root = root(text)?;
    let unit = unit(text);
    let Some(member) = root.members.iter().find(|member| member.key == group) else {
        let mut holder = serde_json::Map::new();
        holder.insert(name.to_string(), value.clone());
        return Some(insert(text, &root, group, &Value::Object(holder), ""));
    };
    let servers = object(text, member.value.0)?;
    let group_indent = line_indent(text, member.start).unwrap_or_default();
    match servers.members.iter().find(|entry| entry.key == name) {
        Some(entry) => {
            let compact = line_indent(text, entry.start).is_none();
            let indent = line_indent(text, entry.start).unwrap_or_default();
            Some(format!(
                "{}{}{}",
                &text[..entry.value.0],
                written(value, &unit, indent, compact),
                &text[entry.value.1..]
            ))
        }
        None => Some(insert(text, &servers, name, value, group_indent)),
    }
}

/// The text without member `index` of `object`.
fn without(text: &str, object: &Object, index: usize) -> String {
    let members = &object.members;
    if members.len() == 1 {
        return format!("{}{}", &text[..object.open + 1], &text[object.close..]);
    }
    // A member on lines of its own goes with its lines (a comment at the
    // end of its last line too); comments on other lines stay. The last
    // member without a trailing comma also takes the comma after the
    // member before it.
    let bytes = text.as_bytes();
    let member = &members[index];
    if line_indent(text, member.start).is_some()
        && let Some((comma, end)) = rest_of_line(bytes, member.value.1)
        && end < text.len()
    {
        let start = line_start(text, member.start);
        if comma.is_some() {
            return format!("{}{}", &text[..start], &text[end + 1..]);
        }
        if index + 1 == members.len()
            && let Some((Some(before), _)) = rest_of_line(bytes, members[index - 1].value.1)
            && before < start
        {
            return format!(
                "{}{}{}",
                &text[..before],
                &text[before + 1..start],
                &text[end + 1..]
            );
        }
    }
    if index == 0 {
        return format!("{}{}", &text[..members[0].start], &text[members[1].start..]);
    }
    format!(
        "{}{}",
        &text[..members[index - 1].value.1],
        &text[members[index].value.1..]
    )
}

/// `text` without `text[group][name]`, every other byte kept; `group` goes
/// too when that empties it. `None` when the member is not there or the
/// text cannot be edited in place.
pub fn remove(text: &str, group: &str, name: &str) -> Option<String> {
    let root = root(text)?;
    let at = root.members.iter().position(|member| member.key == group)?;
    let servers = object(text, root.members[at].value.0)?;
    let index = servers.members.iter().position(|entry| entry.key == name)?;
    if servers.members.len() == 1 {
        return Some(without(text, &root, at));
    }
    Some(without(text, &servers, index))
}

/// `value` as a whole file in the style of `like` (the file it replaces):
/// its indentation unit, and a final newline when it had one (always for a
/// new file).
pub fn render(value: &Value, like: Option<&str>) -> String {
    let unit = like.map_or_else(|| "  ".to_string(), unit);
    let mut text = written(value, &unit, "", false);
    if like.is_none_or(|like| like.ends_with('\n')) {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> Value {
        json!({"command":"graphicle","args":["mcp","--project","."]})
    }

    #[test]
    fn an_entry_is_added_and_removed_without_touching_the_rest() {
        let original = "{\n    \"mcpServers\": {\n        \"docs\": {\"command\": \"node\", \"args\": [\"--docs\"]}\n    },\n    \"other\": true\n}\n";
        let added = set(original, "mcpServers", "graphicle", &entry()).unwrap();
        assert!(
            added.starts_with(
                "{\n    \"mcpServers\": {\n        \"docs\": {\"command\": \"node\", \"args\": [\"--docs\"]},\n        \"graphicle\": {\n            \"command\": \"graphicle\",\n"
            ),
            "{added}"
        );
        assert!(
            added.ends_with("    },\n    \"other\": true\n}\n"),
            "{added}"
        );
        let value = parse(&added).unwrap();
        assert_eq!(value["mcpServers"]["graphicle"], entry());
        assert_eq!(remove(&added, "mcpServers", "graphicle").unwrap(), original);
        // Replaced in place.
        let replaced = set(&added, "mcpServers", "graphicle", &json!({"command":"x"})).unwrap();
        assert_eq!(
            parse(&replaced).unwrap()["mcpServers"]["graphicle"],
            json!({"command":"x"})
        );
        assert!(replaced.contains("\"args\": [\"--docs\"]"));
    }

    #[test]
    fn compact_files_stay_compact_and_groups_come_and_go() {
        let original = r#"{"mcpServers":{"other":{"command":"x"}},"globalShortcut":""}"#;
        let added = set(original, "mcpServers", "graphicle", &entry()).unwrap();
        assert_eq!(
            added,
            r#"{"mcpServers":{"other":{"command":"x"},"graphicle":{"command":"graphicle","args":["mcp","--project","."]}},"globalShortcut":""}"#
        );
        assert_eq!(remove(&added, "mcpServers", "graphicle").unwrap(), original);
        // A missing group is added, and goes again with its last entry.
        let bare = "{\n  \"theme\": \"dark\"\n}\n";
        let grouped = set(bare, "servers", "graphicle", &entry()).unwrap();
        assert_eq!(parse(&grouped).unwrap()["servers"]["graphicle"], entry());
        assert!(
            grouped
                .starts_with("{\n  \"theme\": \"dark\",\n  \"servers\": {\n    \"graphicle\": {\n"),
            "{grouped}"
        );
        assert_eq!(remove(&grouped, "servers", "graphicle").unwrap(), bare);
        // An empty object gets the entry inside.
        let empty = "{\n  \"servers\": {}\n}\n";
        let filled = set(empty, "servers", "graphicle", &entry()).unwrap();
        assert_eq!(parse(&filled).unwrap()["servers"]["graphicle"], entry());
        assert!(filled.ends_with("\n  }\n}\n"), "{filled}");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_kept() {
        let original = "// my servers\n{\n  \"servers\": {\n    /* docs */ \"docs\": {\"command\": \"node\"},\n  },\n}\n";
        assert!(has_comments(original));
        assert!(!has_comments("{\"a\": [1,],}"));
        assert_eq!(
            parse(original).unwrap()["servers"]["docs"]["command"],
            "node"
        );
        let added = set(original, "servers", "graphicle", &entry()).unwrap();
        assert!(added.starts_with("// my servers\n{\n  \"servers\": {\n    /* docs */ \"docs\": {\"command\": \"node\"},\n    \"graphicle\": {"), "{added}");
        assert_eq!(parse(&added).unwrap()["servers"]["graphicle"], entry());
        assert_eq!(remove(&added, "servers", "graphicle").unwrap(), original);
        assert_eq!(error_place("{\"a\": }"), "line 1, column 7");
        assert!(parse("{\"a\": }").is_none());
    }

    #[test]
    fn comments_stay_where_the_user_wrote_them() {
        // A comment at the end of the last entry's line stays on that line,
        // and taking the entry out again gives the text back.
        let original = "{\n  \"servers\": {\n    // Docs.\n    \"docs\": {\"command\": \"node\"} // from npm\n  }\n}\n";
        let added = set(original, "servers", "graphicle", &entry()).unwrap();
        assert!(
            added.starts_with("{\n  \"servers\": {\n    // Docs.\n    \"docs\": {\"command\": \"node\"}, // from npm\n    \"graphicle\": {\n"),
            "{added}"
        );
        assert!(added.ends_with("\n    }\n  }\n}\n"), "{added}");
        assert_eq!(parse(&added).unwrap()["servers"]["graphicle"], entry());
        assert_eq!(remove(&added, "servers", "graphicle").unwrap(), original);
        // Plain JSON stays plain JSON: no trailing comma is left behind.
        let plain = "{\n  \"mcpServers\": {\n    \"docs\": {\"command\": \"node\"}\n  }\n}\n";
        let added = set(plain, "mcpServers", "graphicle", &entry()).unwrap();
        assert!(serde_json::from_str::<Value>(&added).is_ok(), "{added}");
        assert_eq!(remove(&added, "mcpServers", "graphicle").unwrap(), plain);
        // An entry before others goes with its own lines only: the comment
        // above the next entry stays.
        let first = "{\n  \"servers\": {\n    \"graphicle\": {\"command\": \"graphicle\"},\n    // Docs.\n    \"docs\": {\"command\": \"node\"}\n  }\n}\n";
        assert_eq!(
            remove(first, "servers", "graphicle").unwrap(),
            "{\n  \"servers\": {\n    // Docs.\n    \"docs\": {\"command\": \"node\"}\n  }\n}\n"
        );
        // A comma that starts the next line: the text as before this change.
        let leading = "{\"servers\": {\n  \"graphicle\": 1\n  , \"b\": 2\n}}";
        assert_eq!(
            parse(&remove(leading, "servers", "graphicle").unwrap()).unwrap(),
            serde_json::json!({"servers":{"b":2}})
        );
        // In the middle, with comments around it.
        let middle = "{\"servers\": {\n  \"a\": 1, // one\n  /* ours */ \"graphicle\": {\n    \"command\": \"g\"\n  }, // ours too\n  // b\n  \"b\": 2\n}}";
        assert_eq!(
            remove(middle, "servers", "graphicle").unwrap(),
            "{\"servers\": {\n  \"a\": 1, // one\n  // b\n  \"b\": 2\n}}"
        );
        assert_eq!(
            comments("// a\n{\"x\": \"// not\", /* b */ \"y\": 1}"),
            ["// a", "/* b */"]
        );
    }

    #[test]
    fn a_whole_file_keeps_its_indentation_and_final_newline() {
        let value = json!({"a":{"b":[1]}});
        assert_eq!(
            render(&value, None),
            "{\n  \"a\": {\n    \"b\": [\n      1\n    ]\n  }\n}\n"
        );
        assert_eq!(
            render(&value, Some("{\n\t\"x\": 1\n}")),
            "{\n\t\"a\": {\n\t\t\"b\": [\n\t\t\t1\n\t\t]\n\t}\n}"
        );
    }
}
