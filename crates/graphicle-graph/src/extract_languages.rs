// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! File wrappers, source normalization and rationale extraction from extract.py.
use crate::{
    Result,
    extractors::{
        base::{file_stem, make_id, read_source_bytes, read_source_text, read_text},
        engine, engine_config,
        engine_types::{KindName, children},
        resolution_facts::walk,
        resolution_js as js,
    },
    io_compat,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, sync::LazyLock};
use tree_sitter::Node;
pub fn suffix(path: &Path) -> String {
    path.extension()
        .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}
pub fn generic(path: &Path, language: &str, source: Option<&[u8]>) -> Value {
    match engine_config::config(language) {
        Ok(c) => engine::extract_generic(path, &c, source),
        Err(_) => json!({"nodes":[],"edges":[],"error":format!("Unknown parser: {language}")}),
    }
}
fn arr<'a>(r: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    if !r[key].is_array() {
        r[key] = json!([]);
    }
    r[key].as_array_mut().unwrap()
}
fn node(id: &str, label: &str, path: &Path, line: usize, kind: &str) -> Value {
    json!({"id":id,"label":label,"file_type":kind,"source_file":path,"source_location":format!("L{line}")})
}
fn edge(src: &str, tgt: &str, rel: &str, path: &Path, line: usize) -> Value {
    json!({"source":src,"target":tgt,"relation":rel,"confidence":"EXTRACTED","source_file":path,"source_location":format!("L{line}"),"weight":1.0})
}
/// Port of `graphify.extract._shorten_rationale_label`, i.e. Python's
/// `textwrap.shorten(text, width, placeholder="…")` (a `TextWrapper` with
/// `max_lines=1`, `break_long_words=True`, `break_on_hyphens=True`) plus the
/// upstream fallback to plain character truncation when the result would be
/// content-free. Lengths are in code points, like Python `len`.
/// Credential-shaped text is redacted first (Graphify 0.9.93, #4566): a token
/// cut by the truncation could not be recognized afterwards.
pub fn shorten_rationale(text: &str, width: usize) -> String {
    let text = crate::security::redact_secrets(text);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let label = textwrap_shorten(&flat, width, '…');
    if label.is_empty() || label == "…" {
        if flat.chars().count() <= width {
            return flat;
        }
        let mut out: String = flat.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        return out;
    }
    label
}
fn tw_is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
fn tw_is_letter(c: char) -> bool {
    tw_is_word(c) && !c.is_numeric()
}
fn tw_is_word_punct(c: char) -> bool {
    tw_is_word(c) || matches!(c, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}
/// `TextWrapper._split` with `break_on_hyphens=True` (`wordsep_re`) applied
/// to already-flattened text (single spaces, no tabs). Returns chunks as
/// `Vec<char>` so lengths follow Python code-point semantics.
fn tw_split(chars: &[char]) -> Vec<Vec<char>> {
    let n = chars.len();
    let mut out = Vec::new();
    let mut s = 0;
    while s < n {
        if chars[s] == ' ' {
            let mut e = s;
            while e < n && chars[e] == ' ' {
                e += 1;
            }
            out.push(chars[s..e].to_vec());
            s = e;
            continue;
        }
        // em-dash between words: (?<=wp) -{2,} (?=\w)
        if chars[s] == '-' && s > 0 && tw_is_word_punct(chars[s - 1]) {
            let mut e = s;
            while e < n && chars[e] == '-' {
                e += 1;
            }
            if e - s >= 2 && e < n && tw_is_word(chars[e]) {
                out.push(chars[s..e].to_vec());
                s = e;
                continue;
            }
        }
        // word, possibly hyphenated: nws+? followed by one of three endings
        let mut e = s + 1;
        loop {
            let at_end = e == n || chars[e] == ' ';
            if at_end {
                break;
            }
            let hyphen_split = chars[e - 1] == '-'
                && ((e >= 3 && tw_is_letter(chars[e - 3]) && tw_is_letter(chars[e - 2]))
                    || (e >= 4
                        && tw_is_letter(chars[e - 4])
                        && chars[e - 3] == '-'
                        && tw_is_letter(chars[e - 2])))
                && tw_is_letter(chars[e])
                && ((e + 1 < n && tw_is_letter(chars[e + 1]))
                    || (e + 2 < n && chars[e + 1] == '-' && tw_is_letter(chars[e + 2])));
            if hyphen_split {
                break;
            }
            let em_dash = tw_is_word_punct(chars[e - 1]) && chars[e] == '-' && {
                let mut k = e;
                while k < n && chars[k] == '-' {
                    k += 1;
                }
                k - e >= 2 && k < n && tw_is_word(chars[k])
            };
            if em_dash {
                break;
            }
            e += 1;
        }
        out.push(chars[s..e].to_vec());
        s = e;
    }
    out
}
fn tw_blank(c: &[char]) -> bool {
    c.iter().all(|x| *x == ' ')
}
/// `TextWrapper(width, max_lines=1, placeholder).fill(text)` for flattened text.
fn textwrap_shorten(flat: &str, width: usize, placeholder: char) -> String {
    let chars: Vec<char> = flat.chars().collect();
    let mut chunks = tw_split(&chars);
    chunks.reverse();
    let mut cur: Vec<Vec<char>> = Vec::new();
    let mut cur_len = 0usize;
    while let Some(l) = chunks.last().map(Vec::len) {
        if cur_len + l <= width {
            cur.push(chunks.pop().unwrap());
            cur_len += l;
        } else {
            break;
        }
    }
    if chunks.last().is_some_and(|c| c.len() > width) {
        // _handle_long_word with break_long_words=True, break_on_hyphens=True
        let space_left = if width < 1 { 1 } else { width - cur_len };
        let chunk = chunks.pop().unwrap();
        let mut end = space_left.min(chunk.len());
        if chunk.len() > space_left
            && let Some(h) = chunk[..space_left].iter().rposition(|c| *c == '-')
            && h > 0
            && chunk[..h].iter().any(|c| *c != '-')
        {
            end = h + 1;
        }
        cur.push(chunk[..end].to_vec());
        chunks.push(chunk[end..].to_vec());
        cur_len = cur.iter().map(Vec::len).sum();
    }
    if cur.last().is_some_and(|c| tw_blank(c)) {
        cur_len -= cur.last().unwrap().len();
        cur.pop();
    }
    if cur.is_empty() {
        return String::new();
    }
    let only_trailing_blank = chunks.len() == 1 && tw_blank(&chunks[0]);
    if (chunks.is_empty() || only_trailing_blank) && cur_len <= width {
        return cur.concat().into_iter().collect();
    }
    while let Some(last) = cur.last() {
        if !tw_blank(last) && cur_len < width {
            let mut s: String = cur.concat().into_iter().collect();
            s.push(placeholder);
            return s;
        }
        cur_len -= last.len();
        cur.pop();
    }
    placeholder.to_string()
}
pub fn autogenerated_python(source: &[u8]) -> bool {
    static REVISION: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^revision\s*[:=]").unwrap());
    let head = String::from_utf8_lossy(&source[..source.len().min(2048)]);
    [
        "DO NOT EDIT",
        "@generated",
        "Generated by the protocol buffer",
    ]
    .iter()
    .any(|s| head.contains(s))
        || (head.contains("def upgrade(")
            && head.contains("down_revision")
            && REVISION.is_match(&head))
        || (head.contains("class Migration(migrations.Migration)") && head.contains("operations"))
}
fn add_rationale(path: &Path, r: &mut Value, text: &str, line: usize, parent: &str) {
    let id = make_id([
        file_stem(path).as_str(),
        "rationale",
        line.to_string().as_str(),
    ]);
    if !arr(r, "nodes").iter().any(|n| n["id"] == id) {
        arr(r, "nodes").push(node(
            &id,
            &shorten_rationale(text, 80),
            path,
            line,
            "rationale",
        ));
    }
    arr(r, "edges").push(edge(&id, parent, "rationale_for", path, line));
}
fn docstring(n: Node<'_>, s: &[u8]) -> Option<(String, usize)> {
    for c in children(n) {
        if c.kind_name() == "comment" {
            continue;
        }
        if c.kind_name() == "expression_statement" {
            for sub in children(c) {
                if matches!(sub.kind_name(), "string" | "concatenated_string") {
                    let t = read_text(sub, s)
                        .trim_matches(['\'', '"'])
                        .trim()
                        .to_string();
                    if t.chars().count() > 20 {
                        return Some((t, c.start_position().row + 1));
                    }
                }
            }
        }
        break;
    }
    None
}
pub fn python_rationale(path: &Path, r: &mut Value) {
    let Some(parsed) =
        crate::extractors::parse_cache::parse(path, "python", tree_sitter_python::LANGUAGE.into())
    else {
        return;
    };
    let source: &[u8] = &parsed.source;
    let root = parsed.tree.root_node();
    let file = make_id([path.to_string_lossy().as_ref()]);
    if !autogenerated_python(source)
        && let Some((text, line)) = docstring(root, source)
    {
        add_rationale(path, r, &text, line, &file);
    }
    // Every node waits on the stack with its owner's id; the ids are shared
    // rather than copied per node.
    let file_id: std::rc::Rc<str> = file.as_str().into();
    let mut stack = vec![(root, file_id)];
    while let Some((n, parent)) = stack.pop() {
        let kind = n.kind_name();
        if matches!(kind, "class_definition" | "function_definition") {
            if let (Some(name), Some(body)) =
                (n.child_by_field_name("name"), n.child_by_field_name("body"))
            {
                let name = read_text(name, source);
                // A nested class's id hangs off its enclosing class, as a
                // method's does (#4228).
                let id = if *parent == *file {
                    make_id([file_stem(path).as_str(), name.as_str()])
                } else {
                    make_id([&*parent, name.as_str()])
                };
                if let Some((text, line)) = docstring(body, source) {
                    add_rationale(path, r, &text, line, &id);
                }
                if kind == "class_definition" {
                    let id: std::rc::Rc<str> = id.into();
                    stack.extend(children(body).into_iter().rev().map(|n| (n, id.clone())));
                }
            }
            continue;
        }
        stack.extend(children(n).into_iter().rev().map(|n| (n, parent.clone())));
    }
    // `# {tag}:` for each rationale tag, spelled out once instead of per line.
    const TAGS: [&str; 7] = [
        "# NOTE:",
        "# IMPORTANT:",
        "# HACK:",
        "# WHY:",
        "# RATIONALE:",
        "# TODO:",
        "# FIXME:",
    ];
    for (i, line) in String::from_utf8_lossy(source).lines().enumerate() {
        let t = line.trim();
        if TAGS.iter().any(|p| t.starts_with(p)) {
            add_rationale(path, r, t, i + 1, &file);
        }
    }
}
pub fn js_rationale(path: &Path, r: &mut Value) {
    static REFERENCES: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)\b(ADR[- ]?\d{1,5}|RFC[- ]?\d{1,5})\b").unwrap());
    let Ok(source) = read_source_text(path, false) else {
        return;
    };
    let file = make_id([path.to_string_lossy().as_ref()]);
    let mut seen = BTreeSet::new();
    // `// {tag}:` and `* {tag}:` for each rationale tag, spelled out once.
    const TAGS: [&str; 14] = [
        "// NOTE:",
        "* NOTE:",
        "// IMPORTANT:",
        "* IMPORTANT:",
        "// HACK:",
        "* HACK:",
        "// WHY:",
        "* WHY:",
        "// RATIONALE:",
        "* RATIONALE:",
        "// TODO:",
        "* TODO:",
        "// FIXME:",
        "* FIXME:",
    ];
    for (i, line) in source.lines().enumerate() {
        let t = line.trim();
        if TAGS.iter().any(|p| t.starts_with(p)) {
            add_rationale(path, r, t.trim_start_matches(['/', '*', ' ']), i + 1, &file);
        }
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            for cap in REFERENCES.captures_iter(t) {
                let tok = cap[1].to_uppercase();
                let kind = &tok[..3];
                let num = tok[3..].trim_start_matches(['-', ' ']);
                let label = if kind == "ADR" {
                    format!("ADR-{num:0>4}")
                } else {
                    format!("RFC-{num}")
                };
                if !seen.insert(label.clone()) {
                    continue;
                }
                let id = make_id(["docref", label.as_str()]);
                if !arr(r, "nodes").iter().any(|n| n["id"] == id) {
                    arr(r, "nodes").push(node(&id, &label, path, i + 1, "doc_ref"));
                }
                arr(r, "edges").push(edge(&file, &id, "cites", path, i + 1));
            }
        }
    }
}
fn is_code(mut n: Node<'_>) -> bool {
    let mut substitution = false;
    loop {
        if matches!(n.kind(), "comment" | "string" | "regex" | "regex_pattern") {
            return false;
        }
        if n.kind() == "template_substitution" {
            substitution = true;
        } else if n.kind() == "template_string" && !substitution {
            return false;
        }
        if let Some(p) = n.parent() {
            n = p
        } else {
            break;
        }
    }
    true
}
/// `_normalize_ts_export_type_star`: blank the `type` of TS 5.0's
/// `export type * from "..."`, which tree-sitter-typescript 0.23 rejects (the
/// whole file was dropped), so it parses as the plain `export * from "..."`
/// re-export; every other byte and offset is kept (#3942). `None` when the
/// file has no such statement.
pub fn normalize_ts_export_type_star(source: &[u8]) -> Option<Vec<u8>> {
    static RE: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(r"(?-u)\bexport\s+(type)\s*\*").unwrap());
    let mut masked: Option<Vec<u8>> = None;
    for c in RE.captures_iter(source) {
        let word = c.get(1).unwrap().range();
        masked.get_or_insert_with(|| source.to_vec())[word].fill(b' ');
    }
    masked
}
pub fn normalize_ts_import_types(source: &[u8], tsx: bool) -> Option<Vec<u8>> {
    // Compiled once per process rather than per file.
    static RE: LazyLock<Option<regex::bytes::Regex>> = LazyLock::new(|| {
        regex::bytes::Regex::new(r#"\bimport\s*\(\s*['\"][^'\"\r\n]+['\"]\s*\)"#).ok()
    });
    let re = RE.as_ref()?;
    let matches = re.find_iter(source).collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(
            &if tsx {
                tree_sitter_typescript::LANGUAGE_TSX
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT
            }
            .into(),
        )
        .ok()?;
    let original = parser.parse(source, None)?;
    let orig = original.root_node();
    let ranges = walk(orig)
        .into_iter()
        .filter(|n| n.kind() == "type_arguments")
        .map(|n| (n.start_byte(), n.end_byte()))
        .collect::<Vec<_>>();
    let matches = matches
        .into_iter()
        .filter(|m| {
            orig.descendant_for_byte_range(m.start(), m.start() + 1)
                .is_none_or(is_code)
                && !ranges
                    .iter()
                    .any(|(a, b)| *a <= m.start() && m.start() < *b)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    let mut masked = source.to_vec();
    let blank = |bytes: &mut [u8]| {
        for b in bytes.iter_mut() {
            if !matches!(*b, b'\n' | b'\r') {
                *b = b' ';
            }
        }
        if let Some(b) = bytes.first_mut() {
            *b = b'T';
        }
    };
    for m in &matches {
        blank(&mut masked[m.range()]);
    }
    let tree = parser.parse(&masked, None)?;
    let ranges = walk(tree.root_node())
        .into_iter()
        .filter(|n| {
            n.kind() == "type_arguments"
                && n.parent()
                    .is_some_and(|p| matches!(p.kind(), "call_expression" | "new_expression"))
        })
        .map(|n| (n.start_byte(), n.end_byte()))
        .collect::<Vec<_>>();
    let errors = walk(orig)
        .into_iter()
        .filter(|n| n.kind() == "ERROR" || n.is_error())
        .collect::<Vec<_>>();
    let mut out = source.to_vec();
    let mut changed = false;
    for m in matches {
        if ranges.iter().any(|(a, b)| {
            *a <= m.start()
                && m.start() < *b
                && (orig.descendant_for_byte_range(*a, *a + 1).is_none_or(|n| {
                    n.kind() != "<" || n.parent().is_none_or(|p| p.kind() != "binary_expression")
                }) || errors
                    .iter()
                    .any(|e| e.start_byte() <= b + 2 && e.end_byte() >= b.saturating_sub(1)))
        }) {
            blank(&mut out[m.range()]);
            changed = true;
        }
    }
    changed.then_some(out)
}
fn blank_text(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '\n' | '\r') { c } else { ' ' })
        .collect()
}
pub fn normalize_cpp_cli(source: &[u8]) -> Option<Vec<u8>> {
    static CLI: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\b(?:ref|value)\s+(?:class|struct)\b|\binterface\s+class\b|\bgcnew\b|\[\s*(?:assembly|module)\s*:").unwrap()
    });
    let text = String::from_utf8_lossy(source);
    if !CLI.is_match(&text) {
        return None;
    }
    let mut out = text.into_owned();
    for pat in [
        r"(?:\b(?:public|private|protected)\s+)?\b(?:ref|value)\s+(?=(?:class|struct)\b)",
        r"(?:\b(?:public|private|protected)\s+)?\binterface\s+(?=class\b)",
    ] {
        let re = fancy_regex::Regex::new(pat).unwrap();
        out = re
            .replace_all(&out, |c: &fancy_regex::Captures| {
                blank_text(c.get(0).unwrap().as_str())
            })
            .into_owned();
    }
    out = regex::Regex::new(r"\bgcnew\b")
        .unwrap()
        .replace_all(&out, "new  ")
        .into_owned();
    let re = fancy_regex::Regex::new(r"(?<=[A-Za-z0-9_>])[\^%](?=\s*[,)\]>;])").unwrap();
    out = re.replace_all(&out, " ").into_owned();
    let re=fancy_regex::Regex::new(r"(\b[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+|\b[A-Z][A-Za-z0-9_]*[a-z][A-Za-z0-9_]*|\b[A-Z](?=[\^%])|\b(?:bool|char|wchar_t|short|int|long|float|double|unsigned|signed)|>)[\^%](?=\s+[A-Za-z_])").unwrap();
    out = re.replace_all(&out, "$1 ").into_owned();
    out = regex::Regex::new(r"\[\s*(?:assembly|module)\s*:[^\[\]]*\]")
        .unwrap()
        .replace_all(&out, |c: &regex::Captures| blank_text(&c[0]))
        .into_owned();
    Some(out.into_bytes())
}
/// `_normalize_cpp_export_macros`: blank ALL-CAPS export macros between
/// `class`/`struct` and the type name (`class MODULE_API Widget : Base`),
/// stacked ones included, keeping every newline and byte offset (#3661). The
/// head of a range-based `for(class MACRO v : items)` and brace initialisers
/// (`struct MACRO v{1}`) are left alone; paren-attribute macros and template
/// specialisations are not handled, as upstream documents. The pattern is
/// upstream's bytes regex: its classes are ASCII and `\b` is the ASCII word
/// boundary spelled as a lookbehind.
pub fn normalize_cpp_export_macros(source: &[u8]) -> Vec<u8> {
    static RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(concat!(
            r"(?<![A-Za-z0-9_])((?:class|struct)[ \t\r\n]+)",
            r"((?:[A-Z][A-Z0-9_]*[ \t\r\n]+)+)",
            r"(?!final[ \t\r\n]*[:{])",
            r"([A-Za-z_][A-Za-z0-9_]*(?:[ \t\r\n]+final)?[ \t\r\n]*)([:{])",
        ))
        .unwrap()
    });
    let Ok(text) = std::str::from_utf8(source) else {
        return source.to_vec();
    };
    let space = |b: &u8| matches!(*b, b' ' | b'\t' | b'\r' | b'\n');
    let mut out = Vec::with_capacity(source.len());
    let mut last = 0;
    for c in RE.captures_iter(text) {
        let Ok(c) = c else { break };
        let whole = c.get(0).unwrap();
        out.extend_from_slice(&source[last..whole.start()]);
        last = whole.end();
        // `for(class MACRO var: items)`: preceded by `(`, not a declaration.
        let opens_paren = source[..whole.start()]
            .iter()
            .rev()
            .find(|b| !space(b))
            .is_some_and(|b| *b == b'(');
        // `struct MACRO var{1}`: a brace initialiser, not a class body.
        let initialiser = &c[4] == "{"
            && source[whole.end()..]
                .iter()
                .find(|b| !space(b))
                .is_some_and(|b| b"0123456789\"'-".contains(b));
        if opens_paren || initialiser {
            out.extend_from_slice(whole.as_str().as_bytes());
            continue;
        }
        out.extend_from_slice(c[1].as_bytes());
        out.extend(
            c[2].bytes()
                .map(|b| if matches!(b, b'\r' | b'\n') { b } else { b' ' }),
        );
        out.extend_from_slice(c[3].as_bytes());
        out.extend_from_slice(c[4].as_bytes());
    }
    out.extend_from_slice(&source[last..]);
    out
}
pub fn cpp_string_tests(path: &Path, r: &mut Value) {
    let Ok(source) = read_source_text(path, false) else {
        return;
    };
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"(?m)^[ \t]*(?:TEST_CASE|TEST_CASE_TEMPLATE|SCENARIO)\s*\(\s*"((?:[^"\\]|\\.)+)""#,
        )
        .unwrap()
    });
    let re = &*RE;
    let file = make_id([path.to_string_lossy().as_ref()]);
    let stem = file_stem(path);
    for cap in re.captures_iter(&source) {
        let name = &cap[1];
        let line = source[..cap.get(0).unwrap().start()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1;
        let mut id = make_id([stem.as_str(), name]);
        if id == make_id([stem.as_str()]) {
            id = make_id([stem.as_str(), "test", format!("L{line}").as_str()]);
        }
        if arr(r, "nodes").iter().any(|n| n["id"] == id) {
            continue;
        }
        arr(r, "nodes").push(node(&id, &format!("\"{name}\""), path, line, "code"));
        arr(r, "edges").push(edge(&file, &id, "contains", path, line));
    }
}
/// `_resolve_rescued_specifier`: `(node_id, stub_source_file, resolved_file,
/// is_external)` for a regex-rescued import specifier, resolved the way the
/// AST import handler does. A bare or scoped specifier names a package:
/// `lodash/fp/map` is `lodash` and `@a/utils` stays distinct from `@b/utils`,
/// and for such an external the stub's source file is the package name.
pub fn rescued_specifier(
    path: &Path,
    raw: &str,
) -> Option<(String, String, Option<std::path::PathBuf>, bool)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved = if raw.starts_with('.') {
        Some(js::resolve_js_import_path(&js::norm(&parent.join(raw))))
    } else {
        js::resolve_tsconfig_alias(
            raw,
            &js::load_tsconfig_aliases(parent),
            js::load_tsconfig_base_url(parent).as_deref(),
        )
        .map(|p| js::resolve_js_import_path(&p))
        .or_else(|| {
            if raw.starts_with("@/") {
                js::resolve_js_module_path(raw, Some(parent)).filter(|p| p.is_file())
            } else {
                None
            }
        })
    };
    if let Some(p) = resolved {
        return Some((
            make_id([p.to_string_lossy().as_ref()]),
            p.to_string_lossy().into_owned(),
            p.is_file().then_some(p),
            false,
        ));
    }
    let parts = raw.split('/').collect::<Vec<_>>();
    let module = if raw.starts_with('@') && parts.len() >= 2 {
        parts[..2].join("/")
    } else {
        parts[0].to_string()
    };
    if module.is_empty() {
        return None;
    }
    Some((make_id([module.as_str()]), module, None, true))
}
/// `_emit_rescued_import`. An external stub is a module anchor (`type`
/// `module`, exempt from id disambiguation, so it collapses onto a manifest's
/// declaration of the same package) named by its package (#3084).
fn rescued_import(path: &Path, r: &mut Value, raw: &str, relation: &str) {
    let Some((id, source, target, external)) = rescued_specifier(path, raw) else {
        return;
    };
    let mut e = json!({"source":make_id([path.to_string_lossy().as_ref()]),"target":id,"relation":relation,"confidence":"EXTRACTED","source_file":path});
    if let Some(p) = target {
        e["target_file"] = json!(p);
    } else if !arr(r, "nodes").iter().any(|n| n["id"] == id) {
        let mut stub = json!({"id":id,"label":raw,"file_type":"code","source_file":source,"confidence":"EXTRACTED"});
        if external {
            stub["type"] = json!("module");
            stub["label"] = json!(source);
        }
        arr(r, "nodes").push(stub);
    }
    arr(r, "edges").push(e);
}
pub fn rescue_dynamic(path: &Path, r: &mut Value) {
    let Ok(src) = read_source_text(path, false) else {
        return;
    };
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"\bimport\s*\(\s*(?:'([^'\n]+)'|"([^"\n]+)"|`([^`$\n]+)`)\s*\)"#)
            .unwrap()
    });
    let re = &*RE;
    let file = make_id([path.to_string_lossy().as_ref()]);
    let mut ids = BTreeSet::new();
    let mut files = BTreeSet::new();
    for e in arr(r, "edges").iter() {
        if e["deferred"] == true && e["relation"] == "imports_from" && e["source"] == file {
            if let Some(id) = e["target"].as_str() {
                ids.insert(id.to_string());
            }
            if let Some(p) = e["target_file"].as_str() {
                files.insert(crate::extractors::resolution::resolved(Path::new(p)));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for cap in re.captures_iter(&src) {
        let start = cap.get(0).unwrap().start();
        if src[src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0)..start].contains("//") {
            continue;
        }
        let raw = (1..=3).find_map(|i| cap.get(i)).unwrap().as_str();
        let Some((id, _, p, _)) = rescued_specifier(path, raw) else {
            continue;
        };
        if ids.contains(&id)
            || ids.contains(&make_id(["ref", raw]))
            || p.as_ref()
                .is_some_and(|p| files.contains(&crate::extractors::resolution::resolved(p)))
        {
            continue;
        }
        let key = p
            .as_ref()
            .map(|p| {
                crate::extractors::resolution::resolved(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| raw.into());
        if seen.insert(key) {
            rescued_import(path, r, raw, "dynamic_import");
        }
    }
}
/// `_php_blank_unparsed_casts`: tree-sitter-php has no PHP 8.5 `(void)` cast
/// and cannot parse a cast applied directly to `match (...)`, so the statement
/// became an ERROR node and could lose its call (#4202). Each such cast outside
/// strings and comments is blanked to spaces, keeping every byte offset and
/// line. `None` when nothing was blanked. The patterns are upstream's bytes
/// regex: ASCII whitespace and ASCII case-insensitivity, anchored per `(`.
pub fn php_blank_unparsed_casts(source: &[u8]) -> Option<Vec<u8>> {
    const WS: &str = r"[\t\n\x0B\x0C\r ]";
    static VOID: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(&format!(r"(?i-u)^\({WS}*void{WS}*\)")).unwrap());
    static CAST: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        regex::bytes::Regex::new(&format!(
            r"(?i-u)^\({WS}*(?:string|int|integer|float|double|real|bool|boolean|array|object|binary){WS}*\)"
        ))
        .unwrap()
    });
    static MATCH: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(&format!(r"(?i-u)^{WS}*match{WS}*\(")).unwrap());
    let mut casts = vec![];
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'(' {
            let rest = &source[i..];
            let end = VOID.find(rest).map(|m| m.end()).or_else(|| {
                CAST.find(rest)
                    .filter(|m| MATCH.is_match(&rest[m.end()..]))
                    .map(|m| m.end())
            });
            if let Some(end) = end {
                casts.push(i..i + end);
                i += end;
                continue;
            }
        }
        i += 1;
    }
    if casts.is_empty() {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&graphicle_tree_sitter_php::LANGUAGE_PHP.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    const NON_CODE: [&str; 6] = [
        "comment",
        "string",
        "encapsed_string",
        "heredoc",
        "nowdoc",
        "text",
    ];
    let mut out = source.to_vec();
    let mut changed = false;
    for cast in casts {
        let mut node = root.descendant_for_byte_range(cast.start, cast.start + 1);
        while let Some(n) = node {
            if NON_CODE.contains(&n.kind()) {
                break;
            }
            node = n.parent();
        }
        if node.is_none() {
            for b in &mut out[cast] {
                if !matches!(*b, b'\r' | b'\n') {
                    *b = b' ';
                }
            }
            changed = true;
        }
    }
    changed.then_some(out)
}
/// `_csharp_string_opener`: at `i`, the end of a run of `$`/`@` prefixes, the
/// length of the quote run that opens a string there (0 for none), and whether
/// the prefixes made it interpolated and verbatim.
fn csharp_string_opener(source: &[u8], i: usize) -> (usize, usize, bool, bool) {
    let (mut j, mut interpolated, mut verbatim) = (i, false, false);
    while let Some(&b) = source.get(j) {
        match b {
            b'$' => interpolated = true,
            b'@' => verbatim = true,
            _ => break,
        }
        j += 1;
    }
    if source.get(j) != Some(&b'"') {
        return (j, 0, interpolated, verbatim);
    }
    let run = source[j..].iter().take_while(|&&b| b == b'"').count();
    (j, run, interpolated, verbatim)
}
fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}
/// `_csharp_broken_escape_offsets`: offsets of the `""` escape that ends a
/// verbatim interpolated string right after an interpolation hole (`}""` then
/// the terminator), the one spelling tree-sitter-c-sharp 0.23.5 rejects
/// (#3376). One pass over the file, so a `$@"` inside a comment, a char
/// literal or another string is never taken for code.
fn csharp_broken_escape_offsets(source: &[u8]) -> Vec<usize> {
    let n = source.len();
    let mut offsets = vec![];
    let mut i = 0;
    while i < n {
        if source[i..].starts_with(b"//") {
            i = find_bytes(source, b"\n", i).map_or(n, |nl| nl + 1);
            continue;
        }
        if source[i..].starts_with(b"/*") {
            i = find_bytes(source, b"*/", i + 2).map_or(n, |end| end + 2);
            continue;
        }
        if source[i] == b'\'' {
            let mut j = i + 1;
            while j < n {
                if source[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if matches!(source[j], b'\'' | b'\n') {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        let (quote, run, interpolated, verbatim) = csharp_string_opener(source, i);
        if run == 0 {
            // Resume after the `$`/`@` run: re-walking it from every offset
            // would make a malformed file quadratic.
            i = if quote > i { quote } else { i + 1 };
            continue;
        }
        if run >= 3 {
            // A raw string literal closes with a quote run of the same length.
            i = find_bytes(source, &source[quote..quote + run], quote + run).map_or(n, |e| e + run);
            continue;
        }
        if run == 2 {
            i = quote + 2;
            continue;
        }
        let mut j = quote + 1;
        if verbatim {
            while j < n {
                if source[j..].starts_with(b"\"\"") {
                    j += 2;
                    continue;
                }
                if source[j] == b'"' {
                    break;
                }
                j += 1;
            }
            if j < n
                && interpolated
                && j > quote + 2
                && &source[j - 2..j] == b"\"\""
                && source[j - 3] == b'}'
            {
                offsets.push(j - 2);
            }
        } else {
            while j < n {
                if source[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if matches!(source[j], b'"' | b'\n') {
                    break;
                }
                j += 1;
            }
        }
        i = j + 1;
    }
    offsets
}
/// `_normalize_csharp_verbatim_interpolation`: overwrite with two spaces the
/// `""` escape of each verbatim interpolated string that ends `}""` + `"`,
/// which is valid C# but rejected by tree-sitter-c-sharp 0.23.5, the newest
/// release (#3376). Length preserving, so locations hold; the interpolation
/// hole is kept. `None` unless the file has such a string, fails to parse, and
/// parses cleanly once masked (the cheap scan runs before the parses, which
/// gives the same answer as upstream's parse-first order).
pub fn normalize_csharp_verbatim_interpolation(source: &[u8]) -> Option<Vec<u8>> {
    let offsets = csharp_broken_escape_offsets(source);
    if offsets.is_empty() {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .ok()?;
    if !parser.parse(source, None)?.root_node().has_error() {
        return None;
    }
    let mut masked = source.to_vec();
    for start in offsets {
        masked[start..start + 2].fill(b' ');
    }
    (!parser.parse(&masked, None)?.root_node().has_error()).then_some(masked)
}
/// `_ejs_mask_non_script`: blank an EJS template down to the JavaScript a
/// parser can read. Code tags (`<% %>`, `<%_ %>`) and JS `<script>` bodies are
/// kept; markup, `<%# %>` comments, the tag delimiters and `<%%` literals become
/// spaces, with `\r`/`\n` kept so lines still match. An output tag (`<%= %>`,
/// `<%- %>`) inside kept JS becomes one `0` padded with spaces, so
/// `const x = <%- data %>;` still parses. Upstream works per code point; the
/// marks here are per byte, and every boundary is an ASCII delimiter or a
/// regex match edge, so each character's bytes share one mark. A script's
/// `type` is read with the reviewed attribute parser the Svelte and Astro
/// maskers share, upstream's shared pattern.
pub fn ejs_mask_non_script(src: &str) -> String {
    static SCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<script\b((?:"[^"]*"|'[^']*'|[^>"'])*)>([\s\S]*?)</script\s*>"#)
            .unwrap()
    });
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut keep = vec![false; n];
    let mut neutralize = vec![false; n];
    for c in SCRIPT.captures_iter(src) {
        if !astro_script_is_javascript(&c[1]) {
            continue;
        }
        keep[c.get(2).unwrap().range()].fill(true);
    }
    let mut i = 0;
    while let Some(j) = find_bytes(bytes, b"<%", i) {
        if bytes.get(j + 2) == Some(&b'%') {
            // `<%%`: a literal `<%`.
            keep[j..(j + 3).min(n)].fill(false);
            i = j + 3;
            continue;
        }
        let marker = bytes.get(j + 2).copied();
        let Some(close) = find_bytes(bytes, b"%>", j + 2) else {
            break;
        };
        // The delimiters are never JS, even inside a `<script>` body.
        for p in [j, j + 1, close, close + 1] {
            if p < n {
                keep[p] = false;
            }
        }
        match marker {
            Some(b'=' | b'-') => {
                for p in j + 2..close {
                    if keep[p] {
                        neutralize[p] = true;
                    }
                }
            }
            Some(b'#') => keep[j..close + 2].fill(false),
            _ => {
                // A code tag: its body is JS (`<%_` drops its slurp marker).
                let body = if marker == Some(b'_') {
                    keep[j + 2] = false;
                    j + 3
                } else {
                    j + 2
                };
                if body < close {
                    keep[body..close].fill(true);
                }
            }
        }
        i = close + 2;
    }
    let mut out = String::with_capacity(n);
    let mut in_output = false;
    for (p, ch) in src.char_indices() {
        if ch == '\r' || ch == '\n' {
            out.push(ch);
            in_output = false;
        } else if !keep[p] {
            out.push(' ');
            in_output = false;
        } else if neutralize[p] {
            out.push(if in_output { ' ' } else { '0' });
            in_output = true;
        } else {
            out.push(ch);
            in_output = false;
        }
    }
    out
}
/// `_emit_ejs_include`: the template-composition edge of
/// `<%- include('partial') %>`, probed relative to the including template the
/// way EJS resolves it. A hit is stamped with `target_file` for the
/// canonicalisation pass; an unresolved (views-root or bare) include keeps a
/// stub so the edge is never dropped.
fn ejs_include(path: &Path, r: &mut Value, raw: &str, line: usize) {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved = js::resolve_js_import_path(&js::norm(&parent.join(raw)));
    let mut e = json!({"source":make_id([path.to_string_lossy().as_ref()]),"relation":"includes","confidence":"EXTRACTED","source_file":path,"source_location":format!("L{line}")});
    if resolved.is_file() {
        e["target"] = json!(make_id([resolved.to_string_lossy().as_ref()]));
        e["target_file"] = json!(resolved);
    } else {
        let id = make_id([raw]);
        if !arr(r, "nodes").iter().any(|n| n["id"] == id) {
            arr(r, "nodes").push(json!({"id":id,"label":raw,"file_type":"code","source_file":path,"source_location":format!("L{line}"),"confidence":"EXTRACTED"}));
        }
        e["target"] = json!(id);
    }
    arr(r, "edges").push(e);
}
pub fn php_mask_to_script_blocks(src: &str) -> (String, bool) {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<script\b(?:"[^"]*"|'[^']*'|[^>"'])*>([\s\S]*?)</script\s*>"#)
            .unwrap()
    });
    let re = &*RE;
    let mut out = String::new();
    let mut pos = 0;
    let mut any = false;
    for c in re.captures_iter(src) {
        any = true;
        let body = c.get(1).unwrap();
        let whole = c.get(0).unwrap();
        out.push_str(&blank_text(&src[pos..body.start()]));
        out.push_str(body.as_str());
        out.push_str(&blank_text(&src[body.end()..whole.end()]));
        pos = whole.end();
    }
    if !any {
        return (src.into(), false);
    }
    out.push_str(&blank_text(&src[pos..]));
    (out, true)
}
fn astro_script_is_javascript(attributes: &str) -> bool {
    static ATTRIBUTE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"([^\s=/>"']+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?"#).unwrap()
    });
    for attribute in ATTRIBUTE.captures_iter(attributes) {
        if attribute[1].eq_ignore_ascii_case("type") {
            // HTML keeps the first occurrence of a duplicate attribute.
            let value = (2..=4)
                .find_map(|i| attribute.get(i))
                .map_or("", |m| m.as_str())
                .trim();
            return ["", "module", "text/javascript", "application/javascript"]
                .iter()
                .any(|allowed| value.eq_ignore_ascii_case(allowed));
        }
    }
    true
}

/// `_astro_mask_non_script`: blank everything in a `.astro` file except the
/// frontmatter and JS `<script>` bodies, keeping `\r`/`\n` so AST locations
/// still match the original file. Scripts with a non-JS `type` are blanked too.
/// The reviewed 0.9.73 adaptation parses complete attributes: quoted JS types
/// survive, and `data-type` or an attribute's contents cannot masquerade as `type`.
pub fn astro_mask_non_script(src: &str) -> String {
    static FRONTMATTER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\A\s*---[^\S\r\n]*\r?\n([\s\S]*?)\r?\n---").unwrap());
    static SCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<script\b((?:"[^"]*"|'[^']*'|[^>"'])*)>([\s\S]*?)</script\s*>"#)
            .unwrap()
    });
    let mut keep = vec![];
    let mut from = 0;
    if let Some(c) = FRONTMATTER.captures(src) {
        let body = c.get(1).unwrap();
        keep.push((body.start(), body.end()));
        from = c.get(0).unwrap().end();
    }
    for c in SCRIPT.captures_iter(&src[from..]) {
        if astro_script_is_javascript(&c[1]) {
            let body = c.get(2).unwrap();
            keep.push((from + body.start(), from + body.end()));
        }
    }
    let mut out = String::with_capacity(src.len());
    let mut regions = keep.into_iter().peekable();
    let mut closing = 0;
    for (i, ch) in src.char_indices() {
        while regions.next_if(|&(_, end)| end < i).is_some() {}
        match regions.peek() {
            Some(&(start, end)) if start <= i && i < end => out.push(ch),
            _ if ch == '\r' || ch == '\n' => out.push(ch),
            // Terminate the region in place of the closing `</sc`, as the
            // Svelte masker does (0.9.77, #4072): U+2028 ends a trailing `//`
            // comment without adding a line, `;` ends the statement, and the
            // two fill the 4 bytes they replace. Frontmatter ends at a kept
            // newline and needs no terminator.
            Some(&(_, end)) if end == i => {
                out.push_str("\u{2028};");
                closing = 3;
            }
            _ if closing > 0 => closing -= 1,
            _ => out.push(' '),
        }
    }
    out
}
/// `_svelte_mask_non_script`: blank everything in a `.svelte` file except JS
/// `<script>` bodies, keeping `\r`/`\n` so AST locations still match the
/// original file. Returns the masked source and the first JS block's declared
/// `lang`. `<script module>` / `<script context="module">` blocks are kept with
/// the instance block; scripts with a non-JS `type` are blanked (#3928). The
/// `type` test is the reviewed attribute parser [`astro_script_is_javascript`]
/// shared with the Astro masker, as upstream shares its pattern.
pub fn svelte_mask_non_script(src: &str) -> (String, Option<String>) {
    static SCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<script\b((?:"[^"]*"|'[^']*'|[^>"'])*)>([\s\S]*?)</script\s*>"#)
            .unwrap()
    });
    static LANG: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"(?i)\blang\s*=\s*['"]?([A-Za-z]+)['"]?"#).unwrap());
    let mut keep = vec![];
    let mut lang = None;
    for c in SCRIPT.captures_iter(src) {
        if !astro_script_is_javascript(&c[1]) {
            continue;
        }
        let body = c.get(2).unwrap();
        keep.push((body.start(), body.end()));
        if lang.is_none() {
            lang = LANG.captures(&c[1]).map(|l| l[1].to_lowercase());
        }
    }
    let mut out = String::with_capacity(src.len());
    let mut regions = keep.into_iter().peekable();
    let mut closing = 0;
    for (i, ch) in src.char_indices() {
        while regions.next_if(|&(_, end)| end < i).is_some() {}
        match regions.peek() {
            Some(&(start, end)) if start <= i && i < end => out.push(ch),
            // Terminate the region in place of the closing `</sc`: U+2028 is a
            // JS line terminator, so it ends a trailing `//` comment that would
            // otherwise swallow a second block on the same line, and the `;`
            // ends the statement. It is not a newline, so line numbers hold,
            // and U+2028 plus `;` are 4 UTF-8 bytes, like the 4 they replace.
            Some(&(_, end)) if end == i => {
                out.push_str("\u{2028};");
                closing = 3;
            }
            _ if closing > 0 => closing -= 1,
            _ if ch == '\r' || ch == '\n' => out.push(ch),
            _ => out.push(' '),
        }
    }
    (out, lang)
}
pub fn spock_fallback(path: &Path, r: &Value) -> Value {
    let Ok(source) = read_source_text(path, false) else {
        return r.clone();
    };
    let file = make_id([path.to_string_lossy().as_ref()]);
    let mut out = json!({"nodes":r["nodes"].as_array().into_iter().flatten().filter(|n|n["label"].as_str()==path.file_name().and_then(|s|s.to_str())).take(1).cloned().collect::<Vec<_>>(),"edges":r["edges"].as_array().into_iter().flatten().filter(|e|e["context"]=="import").cloned().collect::<Vec<_>>()});
    if arr(&mut out, "nodes").is_empty() {
        arr(&mut out, "nodes").push(node(
            &file,
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            path,
            1,
            "code",
        ));
    }
    static CLASS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\s*(?:[\w@]+\s+)*class\s+(\w+)").unwrap());
    static FEATURE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"^\s*def\s+(?:"([^"]+)"|'([^']+)')\s*\("#).unwrap());
    static METHOD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\s*def\s+(\w+)\s*\(").unwrap());
    let (class, feature, method) = (&*CLASS, &*FEATURE, &*METHOD);
    let mut owner = String::new();
    for (i, line) in source.lines().enumerate() {
        if let Some(c) = class.captures(line) {
            owner = make_id([file_stem(path).as_str(), &c[1]]);
            arr(&mut out, "nodes").push(node(&owner, &c[1], path, i + 1, "code"));
            arr(&mut out, "edges").push(edge(&file, &owner, "contains", path, i + 1));
            continue;
        }
        if owner.is_empty() {
            continue;
        }
        let (name, label) = if let Some(c) = feature.captures(line) {
            let n = c.get(1).or_else(|| c.get(2)).unwrap().as_str().to_string();
            (n.clone(), format!("\"{n}\""))
        } else if let Some(c) = method.captures(line) {
            let n = c[1].to_string();
            if ["if", "while", "for", "switch", "catch"].contains(&n.as_str()) {
                continue;
            }
            (n.clone(), format!(".{n}()"))
        } else {
            continue;
        };
        let id = make_id([owner.as_str(), name.as_str()]);
        if !arr(&mut out, "nodes").iter().any(|n| n["id"] == id) {
            arr(&mut out, "nodes").push(node(&id, &label, path, i + 1, "code"));
        }
        arr(&mut out, "edges").push(edge(&owner, &id, "method", path, i + 1));
    }
    out
}
/// `rcfile.effective_suffix`: the suffix a file is extracted as, after the
/// project's `.graphifyrc` language overrides (#2961, 0.9.87), with its case
/// kept ([`crate::rcfile::effective_suffix`]). The JS/TS, OCaml and Robot entry
/// points route through here.
pub fn effective_suffix(path: &Path) -> String {
    crate::rcfile::effective_suffix(path)
}
/// The TS grammar's input for `.ts`-family sources: `export type *` blanked
/// (#3942), then syntactic `import(...)` type arguments rewritten; `None` when
/// neither changed anything, so the engine reads the file itself.
fn normalize_ts(source: &[u8], tsx: bool) -> Option<Vec<u8>> {
    let star = normalize_ts_export_type_star(source);
    normalize_ts_import_types(star.as_deref().unwrap_or(source), tsx).or(star)
}
pub fn extract_language(path: &Path, language: &str) -> Value {
    let language = language.to_uppercase();
    let mut source = None;
    let mut lang = language.as_str();
    // The text an SFC/template wrapper read for its own regex passes.
    let mut text = None;
    let read_error =
        |e: std::io::Error| json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e, path)});
    if language == "JS" {
        let ext = effective_suffix(path).to_lowercase();
        lang = if ext == ".tsx" {
            "TSX"
        } else if [".ts", ".mts", ".cts"].contains(&ext.as_str()) {
            "TS"
        } else {
            "JS"
        };
        if lang != "JS"
            && let Ok(s) = read_source_bytes(path, true)
        {
            source = normalize_ts(&s, lang == "TSX");
        }
    } else if language == "CPP" {
        if let Ok(s) = read_source_bytes(path, true) {
            let cli = normalize_cpp_cli(&s);
            source = Some(normalize_cpp_export_macros(cli.as_deref().unwrap_or(&s)));
        }
    } else if language == "PHP" {
        // `(void)` and casts on `match (...)` the grammar cannot parse (#4202).
        if let Ok(s) = read_source_bytes(path, true) {
            source = php_blank_unparsed_casts(&s);
        }
    } else if language == "CSHARP" {
        // Upstream hands the raw bytes to the verbatim-string repair (#3376).
        if let Ok(s) = crate::extractors::parse_cache::read(path) {
            source = normalize_csharp_verbatim_interpolation(&s);
        }
    } else if language == "VUE" {
        let Ok(src) = read_source_text(path, true) else {
            return json!({"nodes":[],"edges":[]});
        };
        let (masked, variant) = js::vue_mask_non_script(&src);
        lang = match variant.as_deref() {
            Some("tsx") => "TSX",
            Some("js" | "jsx") => "JS",
            _ => "TS",
        };
        source = Some(if lang == "JS" {
            masked.into_bytes()
        } else {
            normalize_ts_import_types(masked.as_bytes(), lang == "TSX")
                .unwrap_or_else(|| masked.into_bytes())
        });
        text = Some(src);
    } else if language == "ASTRO" {
        // Only the frontmatter and JS `<script>` bodies are parsed, with the TS
        // grammar (Astro's default); the template is blanked (#2788).
        let Ok(src) = read_source_text(path, true) else {
            return json!({"nodes":[],"edges":[]});
        };
        let masked = astro_mask_non_script(&src);
        lang = "TS";
        source = Some(
            normalize_ts_import_types(masked.as_bytes(), false)
                .unwrap_or_else(|| masked.into_bytes()),
        );
        text = Some(src);
    } else if language == "SVELTE" {
        // Only the `<script>` bodies are parsed; the markup is not JS and made
        // every component a syntax error that hid its symbols (#3928). The
        // grammar follows the first block's `lang`: js/jsx -> JS, else TS.
        let src = match read_source_text(path, true) {
            Ok(src) => src,
            Err(e) => return read_error(e),
        };
        let (masked, variant) = svelte_mask_non_script(&src);
        lang = if matches!(variant.as_deref(), Some("js" | "jsx")) {
            "JS"
        } else {
            "TS"
        };
        source = Some(if lang == "JS" {
            masked.into_bytes()
        } else {
            normalize_ts_import_types(masked.as_bytes(), false)
                .unwrap_or_else(|| masked.into_bytes())
        });
        text = Some(src);
    } else if language == "EJS" {
        // An EJS template is HTML with `<% %>` scriptlets and `<script>`
        // blocks; only those JS regions are parsed, with the TS grammar
        // (#3811, #2511).
        let src = match read_source_text(path, true) {
            Ok(src) => src,
            Err(e) => return read_error(e),
        };
        let masked = ejs_mask_non_script(&src);
        lang = "TS";
        source = Some(
            normalize_ts_import_types(masked.as_bytes(), false)
                .unwrap_or_else(|| masked.into_bytes()),
        );
        text = Some(src);
    }
    let mut r = generic(path, lang, source.as_deref());
    match language.as_str() {
        "PYTHON" if r.get("error").is_none() => {
            python_rationale(path, &mut r);
        }
        "JS" if r.get("error").is_none() => {
            js_rationale(path, &mut r);
            rescue_dynamic(path, &mut r);
        }
        "CPP" => cpp_string_tests(path, &mut r),
        "GROOVY"
            if read_source_text(path, false).is_ok_and(|s| {
                static SPOCK: LazyLock<regex::Regex> =
                    LazyLock::new(|| regex::Regex::new(r#"(?m)^\s*def\s+["']"#).unwrap());
                SPOCK.is_match(&s)
            }) =>
        {
            r = spock_fallback(path, &r);
        }
        "ASTRO" | "SVELTE" | "VUE" | "EJS" => {
            let src = text.unwrap_or_default();
            static DYNAMIC: LazyLock<regex::Regex> =
                LazyLock::new(|| regex::Regex::new(r#"import\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap());
            for c in DYNAMIC.captures_iter(&src) {
                rescued_import(path, &mut r, &c[1], "dynamic_import");
            }
            if language == "EJS" {
                // `<%- include('partial') %>`: the view composition edge.
                static INCLUDE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r#"<%(?:_|[-=])?\s*include\s*\(\s*['"]([^'"]+)['"]"#).unwrap()
                });
                for c in INCLUDE.captures_iter(&src) {
                    let at = c.get(0).unwrap().start();
                    let line = src[..at].bytes().filter(|b| *b == b'\n').count() + 1;
                    ejs_include(path, &mut r, &c[1], line);
                }
            } else if language != "VUE" {
                static SCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"(?i)<script\b[^>]*>([\s\S]*?)</script\s*>").unwrap()
                });
                static FRONT: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r"\A\s*---\s*\r?\n([\s\S]*?)\r?\n---\s*(?:\r?\n|\z)").unwrap()
                });
                static STATIC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                    regex::Regex::new(r#"import\s+(?:[^'"`;]+?\s+from\s+)?['"]([^'"]+)['"]"#)
                        .unwrap()
                });
                let (script, front, static_re) = (&*SCRIPT, &*FRONT, &*STATIC_RE);
                let mut regions = vec![];
                if language == "ASTRO"
                    && let Some(c) = front.captures(&src)
                {
                    regions.push(c[1].to_string());
                }
                regions.extend(script.captures_iter(&src).map(|c| c[1].to_string()));
                for region in regions {
                    for c in static_re.captures_iter(&region) {
                        rescued_import(path, &mut r, &c[1], "imports_from");
                    }
                }
            }
        }
        "PHP" => {
            if let Ok(src) = read_source_text(path, false) {
                let (masked, any) = php_mask_to_script_blocks(&src);
                if any {
                    let extra = generic(path, "JS", Some(masked.as_bytes()));
                    let file = r["nodes"]
                        .get(0)
                        .map(|n| n["id"].clone())
                        .unwrap_or(Value::Null);
                    let mut collide = BTreeSet::new();
                    for n in extra["nodes"].as_array().into_iter().flatten() {
                        if arr(&mut r, "nodes").iter().any(|x| x["id"] == n["id"]) {
                            if n["id"] != file {
                                collide.insert(n["id"].to_string());
                            }
                        } else {
                            arr(&mut r, "nodes").push(n.clone());
                        }
                    }
                    for e in extra["edges"].as_array().into_iter().flatten() {
                        if !collide.contains(&e["source"].to_string())
                            && !collide.contains(&e["target"].to_string())
                        {
                            arr(&mut r, "edges").push(e.clone());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    r
}
/// A request's `bytes` list (development bindings for the byte normalizers).
fn bytes_arg(r: &Value) -> Vec<u8> {
    r["bytes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
        .collect()
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let path = Path::new(r["path"].as_str().unwrap_or(""));
    let source = r["source"].as_str().unwrap_or("");
    Ok(match r["action"].as_str().unwrap_or("extract") {
        "shorten" => json!(shorten_rationale(
            source,
            r["width"].as_u64().unwrap_or(80) as usize
        )),
        "autogenerated" => json!(autogenerated_python(source.as_bytes())),
        "normalize_ts" => json!(
            normalize_ts_import_types(source.as_bytes(), r["tsx"] == true)
                .map(|s| String::from_utf8_lossy(&s).into_owned())
        ),
        "normalize_cpp" => json!(
            normalize_cpp_cli(source.as_bytes()).map(|s| String::from_utf8_lossy(&s).into_owned())
        ),
        "php_mask" => json!(php_mask_to_script_blocks(source)),
        "astro_mask" => json!(astro_mask_non_script(source)),
        "ejs_mask" => json!(ejs_mask_non_script(source)),
        // Byte-level normalizers take the source as a byte list (any bytes).
        "normalize_ts_export_type_star" => {
            json!(normalize_ts_export_type_star(&bytes_arg(r)))
        }
        "normalize_cpp_export_macros" => json!(normalize_cpp_export_macros(&bytes_arg(r))),
        "php_blank_casts" => json!(php_blank_unparsed_casts(&bytes_arg(r))),
        "csharp_verbatim" => json!(normalize_csharp_verbatim_interpolation(&bytes_arg(r))),
        "csharp_broken_escapes" => json!(csharp_broken_escape_offsets(&bytes_arg(r))),
        "rescued_specifier" => json!(rescued_specifier(path, source)),
        // `_read_source_bytes` / `_read_source_text` (warn=False) for the
        // decode comparison in tools/check_extractor_base.py.
        "decode_source" => {
            let bytes = bytes_arg(r);
            let (decoded, guessed) = crate::extractors::base::decode_source(&bytes);
            json!({"bytes": decoded.to_vec(), "guessed": guessed})
        }
        "read_source_text" => match read_source_text(path, false) {
            Ok(text) => json!(text),
            Err(e) => json!({"error": io_compat::read_error(&e, path)}),
        },
        _ => {
            // `extract_python(path, root=...)`: the scan root anchors imports.
            let _root = engine::enter_scan_root(r["scan_root"].as_str().map(Path::new));
            extract_language(path, r["language"].as_str().unwrap_or("PYTHON"))
        }
    })
}
#[cfg(test)]
mod shorten_rationale_tests {
    use super::shorten_rationale;
    /// Expected values come from CPython 3.12 `textwrap.shorten(..., 80, placeholder="…")`
    /// via `graphify.extract._shorten_rationale_label` (httpx docstrings + edge cases).
    #[test]
    fn breaks_on_hyphens_like_textwrap() {
        let cases = [
            (
                "Handlers for Content-Encoding.\n\nSee: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding",
                "Handlers for Content-Encoding. See: https://developer.mozilla.org/en-…",
            ),
            (
                "Guesses the mimetype based on a filename. Defaults to `application/octet-stream`.",
                "Guesses the mimetype based on a filename. Defaults to `application/octet-…",
            ),
            (
                "The Host header should only include the hostname, or hostname:port (for non-default ports).",
                "The Host header should only include the hostname, or hostname:port (for non-…",
            ),
            (
                "Coerce a primitive data type into a string value. Note that we prefer JSON-style 'true'/'false' for boolean values here.",
                "Coerce a primitive data type into a string value. Note that we prefer JSON-…",
            ),
            (
                "All exception classes exposed by HTTPCore are properly mapped to an HTTPX-equivalent exception.",
                "All exception classes exposed by HTTPCore are properly mapped to an HTTPX-…",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(shorten_rationale(text, 80), want, "{text:?}");
        }
    }
    #[test]
    fn fits_and_long_word_and_fallback() {
        assert_eq!(shorten_rationale("  a   b\tc ", 80), "a b c");
        let exact = "x".repeat(80);
        assert_eq!(shorten_rationale(&exact, 80), exact);
        // break_long_words: a single over-long word is cut at width-1 then "…".
        let long = "y".repeat(100);
        assert_eq!(shorten_rationale(&long, 80), format!("{}…", "y".repeat(79)));
        // Long word broken at its last hyphen inside the budget.
        let hy = format!("{}-{}", "a".repeat(50), "b".repeat(50));
        assert_eq!(shorten_rationale(&hy, 80), format!("{}-…", "a".repeat(50)));
        // Em-dash between words is its own chunk and never split.
        assert_eq!(shorten_rationale("alpha--beta", 80), "alpha--beta");
        assert_eq!(shorten_rationale("alpha--beta", 7), "alpha…");
        // Nothing fits at all -> "…" from textwrap -> upstream fallback slice.
        assert_eq!(shorten_rationale("abcdef", 1), "…");
        assert_eq!(shorten_rationale("abcdef", 3), "ab…");
    }
}
