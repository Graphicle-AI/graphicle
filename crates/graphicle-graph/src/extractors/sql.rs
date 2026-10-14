// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/sql.py
//! Native translation of graphify/extractors/sql.py, including recovery semantics.
use super::base::{file_stem, make_id};
use crate::{Result, fsutil, io_compat, unicode15::WORD_RANGES};
use fancy_regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
/// Unqualified definitions live in the first `search_path` schema (PR #3020).
const DEFAULT_SCHEMA: &str = "public";
pub fn norm_ident(name: &str) -> String {
    name.split('.')
        .map(|part| {
            let mut p = trim(part);
            if p.len() >= 2
                && ((p.starts_with('"') && p.ends_with('"'))
                    || (p.starts_with('`') && p.ends_with('`'))
                    || (p.starts_with('[') && p.ends_with(']')))
            {
                p = &p[1..p.len() - 1];
            }
            p.to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(".")
}
fn pair(chars: &[char], start: usize, a: char, b: char) -> bool {
    chars.get(start) == Some(&a) && chars.get(start + 1) == Some(&b)
}
fn blank(chars: &[char], out: &mut String, start: usize, end: usize) {
    out.extend(
        chars[start..end]
            .iter()
            .map(|c| if *c == '\n' { '\n' } else { ' ' }),
    );
}
fn blank_tail_and_carry(chars: &[char], out: &mut String, mut k: usize) -> usize {
    loop {
        let eol = chars[k..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |p| k + p);
        let mut depth = 0usize;
        let mut m = k;
        while m < eol {
            if pair(chars, m, '/', '*') {
                depth += 1;
                m += 2;
            } else if pair(chars, m, '*', '/') {
                depth = depth.saturating_sub(1);
                m += 2;
            } else {
                m += 1;
            }
        }
        blank(chars, out, k, eol);
        k = eol;
        if depth == 0 {
            return k;
        }
        let mut j = k;
        while j < chars.len() && depth > 0 {
            if pair(chars, j, '/', '*') {
                depth += 1;
                j += 2;
            } else if pair(chars, j, '*', '/') {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        blank(chars, out, k, j);
        k = j;
        if k >= chars.len() {
            return k;
        }
    }
}
/// Offsets are Unicode character offsets, matching Python rather than UTF-8 bytes.
pub fn scan_sql(text: &str) -> (String, Vec<(usize, usize)>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut spans = vec![];
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\'' => {
                let mut j = i + 1;
                while j < n && chars[j] != '\n' {
                    if chars[j] == '\'' {
                        if chars.get(j + 1) == Some(&'\'') {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                blank(&chars, &mut out, i, j);
                i = j;
            }
            '"' | '[' => {
                let closer = if chars[i] == '"' { '"' } else { ']' };
                let mut j = i + 1;
                let mut closed = false;
                while j < n && chars[j] != '\n' {
                    if chars[j] == closer {
                        if chars.get(j + 1) == Some(&closer) {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                let span: String = chars[i..j].iter().collect();
                if closed && !span.contains("--") && !span.contains("/*") {
                    spans.push((i, j));
                    out.push_str(&span);
                    i = j;
                } else {
                    i = blank_tail_and_carry(&chars, &mut out, i);
                }
            }
            '-' if pair(&chars, i, '-', '-') => {
                let j = chars[i..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(n, |p| i + p);
                blank(&chars, &mut out, i, j);
                i = j;
            }
            '/' if pair(&chars, i, '/', '*') => {
                let mut depth = 1usize;
                let mut j = i + 2;
                while j < n && depth > 0 {
                    if pair(&chars, j, '/', '*') {
                        depth += 1;
                        j += 2;
                    } else if pair(&chars, j, '*', '/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&chars, &mut out, i, j);
                i = j;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, spans)
}
fn compile(pattern: &str) -> Regex {
    let mut folded = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            folded.push(c);
            if let Some(n) = chars.next() {
                folded.push(n);
            }
        } else if c.is_ascii_alphabetic() {
            folded.push('[');
            folded.push(c.to_ascii_lowercase());
            folded.push(c.to_ascii_uppercase());
            match c.to_ascii_lowercase() {
                'i' => folded.push_str("İı"),
                's' => folded.push('ſ'),
                'k' => folded.push('K'),
                _ => (),
            }
            folded.push(']');
        } else {
            folded.push(c);
        }
    }
    let word = format!(
        "[{}]",
        WORD_RANGES
            .iter()
            .map(|(a, b)| format!(r"\u{{{a:x}}}-\u{{{b:x}}}"))
            .collect::<String>()
    );
    let boundary = r"(?:(?<!\w)(?=\w)|(?<=\w)(?!\w))";
    Regex::new(
        &folded
            .replace(r"\b", boundary)
            .replace(r"\w", &word)
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .expect("verified static SQL expression")
}
struct Patterns {
    routines: Regex,
    triggers: Regex,
    references: Regex,
    fb: Regex,
    for_table: Regex,
    cte: Regex,
    reads: Regex,
    update: Regex,
    table: Regex,
    end: Regex,
    bracket_ident: Regex,
}
static P: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    routines: compile(concat!(
        r"\bCREATE\s+(?:OR\s+(?:REPLACE|ALTER)\s+)?(?:FUNCTION|PROC(?:EDURE)?)\s+",
        r"(?:IF\s+NOT\s+EXISTS\s+)?",
        r#"((?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+)"#,
        r#"(?:\s*\.\s*(?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+))*)"#
    )),
    // `_TRIGGER_RECOVERY_RX` (0.9.71): a trigger with a procedural body lands
    // in an ERROR node; the subject table is the first ON after the name.
    // `(?:.|\n)` is Python's DOTALL `.`.
    triggers: compile(concat!(
        r"\bCREATE\s+(?:OR\s+(?:REPLACE|ALTER)\s+)?TRIGGER\s+",
        r"(?:IF\s+NOT\s+EXISTS\s+)?",
        r#"((?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+)"#,
        r#"(?:\s*\.\s*(?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+))*)"#,
        r"(?:.|\n)*?\bON\s+",
        r#"((?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+)"#,
        r#"(?:\s*\.\s*(?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|[\w$]+))*)"#
    )),
    references: compile(r"\bREFERENCES\s+([\w$]+)"),
    fb: compile(r"^CREATE\s+(?:OR\s+(?:REPLACE|ALTER)\s+)?(PROCEDURE|TRIGGER|FUNCTION)\s+([\w$]+)"),
    for_table: compile(r"\bFOR\s+([\w$]+)"),
    cte: compile(r"(?:\bWITH\s+(?:RECURSIVE\s+)?|,\s*)([\w$]+)\s*(?:\([^()]*\))?\s+AS\s*\("),
    reads: compile(r"\b(?:FROM|JOIN|INTO)\s+([\w$]+)"),
    update: compile(r"\bUPDATE\s+([\w$]+)"),
    table: compile(r"CREATE\s+TABLE\s+([\w$]+)\s*\("),
    end: compile(r"(?:^|\n)(?:CREATE|SET\s+TERM|ALTER)\s"),
    bracket_ident: compile(concat!(
        r#"^(?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|`[^`\n]+`|[\w$]+)"#,
        r#"(?:\s*\.\s*(?:"(?:[^"\n]|"")+"|\[(?:[^\]\n]|\]\])+\]|`[^`\n]+`|[\w$]+))*"#
    )),
});
fn captures<'a>(rx: &Regex, text: &'a str) -> Result<Vec<fancy_regex::Captures<'a>>> {
    rx.captures_iter(text)
        .map(|r| r.map_err(crate::Error::from))
        .collect()
}
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn read(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.byte_range()]).into_owned()
}
/// Reads an identifier verbatim. tree-sitter-sql has no T-SQL bracket delimiters, so
/// `[dbo].[Alpha]` parses as ERROR tokens around an `object_reference` spanning
/// `dbo].[Alpha`; a bracket-touched name is re-read from the source (issue #2712).
fn ident(node: Node<'_>, source: &[u8]) -> String {
    let text = read(node, source);
    let mut start = node.start_byte();
    if start > 0 && source[start - 1] == b'[' {
        start -= 1;
    } else if !text.contains(['[', ']']) {
        return text;
    }
    let tail = String::from_utf8_lossy(&source[start..]);
    match P.bracket_ident.find(&tail) {
        Ok(Some(m)) => m.as_str().to_owned(),
        _ => text,
    }
}
fn object(node: Node<'_>, source: &[u8]) -> Option<String> {
    children(node)
        .find(|n| n.kind() == "object_reference")
        .map(|n| ident(n, source))
}
fn reference(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut found = false;
    for child in children(node) {
        if child.kind() == "keyword_references" {
            found = true;
        } else if found && child.kind() == "object_reference" {
            return Some(ident(child, source));
        }
    }
    None
}
struct Extractor<'a> {
    source: &'a [u8],
    path: String,
    stem: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    tables: HashMap<String, String>,
    /// Function/procedure name → id, for trigger `executes` links (0.9.77).
    routines: HashMap<String, String>,
}
impl Extractor<'_> {
    fn add_node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
            self.edge(&self.file_id.clone(), id, "contains", line);
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn stub(&mut self, name: &str) -> String {
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn resolve(&mut self, name: &str) -> String {
        self.tables
            .get(&norm_ident(name))
            .cloned()
            .unwrap_or_else(|| self.stub(name))
    }
    fn collect_defined(&mut self, node: Node<'_>) {
        if matches!(node.kind(), "create_table" | "create_view")
            && let Some(name) = object(node, self.source).filter(|n| !n.is_empty())
        {
            self.tables.insert(
                norm_ident(&name),
                make_id([self.stem.as_str(), name.as_str()]),
            );
        }
        for child in children(node) {
            self.collect_defined(child);
        }
    }
    fn collect_from_refs(&mut self, node: Node<'_>, caller: &str, cte_names: &HashSet<String>) {
        let mut active = cte_names.clone();
        for child in children(node).filter(|n| n.kind() == "cte") {
            if let Some(n) =
                children(child).find(|n| matches!(n.kind(), "identifier" | "object_reference"))
            {
                active.insert(norm_ident(&ident(n, self.source)));
            }
        }
        if matches!(node.kind(), "from" | "join") {
            for child in children(node).filter(|n| n.kind() == "relation") {
                for item in children(child).filter(|n| n.kind() == "object_reference") {
                    let name = ident(item, self.source);
                    if !active.contains(&norm_ident(&name)) {
                        let id = self.resolve(&name);
                        self.edge(caller, &id, "reads_from", child.start_position().row + 1);
                    }
                }
            }
        }
        for child in children(node) {
            self.collect_from_refs(child, caller, &active);
        }
    }
    fn walk(&mut self, node: Node<'_>) -> Result<()> {
        let line = node.start_position().row + 1;
        match node.kind() {
            "create_table" => {
                if let Some(name) = object(node, self.source).filter(|n| !n.is_empty()) {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.add_node(&id, &name, line);
                    self.tables.insert(norm_ident(&name), id.clone());
                    for col in children(node).filter(|n| n.kind() == "column_definitions") {
                        let has_error = children(col).any(|n| n.kind() == "ERROR");
                        let mut seen_refs = HashSet::new();
                        for cd in children(col) {
                            let mut refs = vec![];
                            if cd.kind() == "column_definition" {
                                if let Some(r) = reference(cd, self.source) {
                                    refs.push(r);
                                }
                            } else if cd.kind() == "constraints" {
                                for constraint in children(cd).filter(|n| n.kind() == "constraint")
                                {
                                    if let Some(r) = reference(constraint, self.source) {
                                        refs.push(r);
                                    }
                                }
                            }
                            for r in refs.into_iter().filter(|r| !r.is_empty()) {
                                let target = self.resolve(&r);
                                self.edge(&id, &target, "references", line);
                                seen_refs.insert(norm_ident(&r));
                            }
                        }
                        if has_error {
                            let text = read(col, self.source);
                            for c in captures(&P.references, &text)? {
                                let r = &c[1];
                                if seen_refs.insert(norm_ident(r)) {
                                    let target = self.resolve(r);
                                    self.edge(&id, &target, "references", line);
                                }
                            }
                        }
                    }
                }
            }
            "create_view" | "create_function" | "create_procedure" => {
                if let Some(name) = object(node, self.source).filter(|n| !n.is_empty()) {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    let label = if node.kind() == "create_view" {
                        name.clone()
                    } else {
                        format!("{name}()")
                    };
                    self.add_node(&id, &label, line);
                    if node.kind() == "create_view" {
                        self.tables.insert(norm_ident(&name), id.clone());
                    } else {
                        self.routines.insert(norm_ident(&name), id.clone());
                    }
                    self.collect_from_refs(node, &id, &HashSet::new());
                }
            }
            "alter_table" => {
                if let Some(name) = object(node, self.source).filter(|n| !n.is_empty()) {
                    let key = norm_ident(&name);
                    let id = if let Some(id) = self.tables.get(&key) {
                        id.clone()
                    } else {
                        let id = self.stub(&name);
                        self.tables.insert(key, id.clone());
                        id
                    };
                    for child in children(node).filter(|n| n.kind() == "add_constraint") {
                        for constraint in children(child).filter(|n| n.kind() == "constraint") {
                            if let Some(r) =
                                reference(constraint, self.source).filter(|r| !r.is_empty())
                            {
                                let target = self.resolve(&r);
                                self.edge(&id, &target, "references", line);
                            }
                        }
                    }
                }
            }
            "create_trigger" => {
                let (mut trigger, mut table) = (None::<String>, None::<String>);
                let (mut after_trigger, mut after_on) = (false, false);
                let (mut routine, mut after_execute) = (None::<String>, false);
                for c in children(node) {
                    if c.kind() == "keyword_trigger" {
                        after_trigger = true;
                    } else if after_trigger && trigger.is_none() && c.kind() == "object_reference" {
                        trigger = Some(ident(c, self.source));
                    // The subject table follows ON (`... AFTER INSERT ON users`),
                    // not FOR: `FOR EACH ROW` carries no table (0.9.71).
                    } else if c.kind() == "keyword_on" {
                        after_on = true;
                    } else if after_on && table.is_none() && c.kind() == "object_reference" {
                        table = Some(ident(c, self.source));
                    // The routine follows EXECUTE FUNCTION/PROCEDURE; the ON
                    // table is already captured by then (0.9.77).
                    } else if c.kind() == "keyword_execute" {
                        after_execute = true;
                    } else if after_execute && routine.is_none() && c.kind() == "object_reference" {
                        routine = Some(ident(c, self.source));
                    }
                }
                if let Some(name) = trigger.filter(|n| !n.is_empty()) {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.add_node(&id, &name, line);
                    if let Some(t) = table.filter(|t| !t.is_empty()) {
                        let target = self.resolve(&t);
                        self.edge(&id, &target, "triggers", line);
                    }
                    // Only a routine defined earlier in this file binds; an
                    // unknown one fails closed instead of minting a stub.
                    if let Some(target) = routine
                        .filter(|r| !r.is_empty())
                        .and_then(|r| self.routines.get(&norm_ident(&r)).cloned())
                    {
                        self.edge(&id, &target, "executes", line);
                    }
                }
            }
            "create_index" => {
                let (mut index, mut table) = (None::<String>, None::<String>);
                let mut after_on = false;
                for c in children(node) {
                    if c.kind() == "keyword_on" {
                        after_on = true;
                    } else if !after_on
                        && index.is_none()
                        && matches!(c.kind(), "identifier" | "literal")
                    {
                        index = Some(read(c, self.source).trim_matches(['"', '`']).into());
                    } else if after_on && table.is_none() && c.kind() == "object_reference" {
                        table = Some(ident(c, self.source));
                    }
                }
                if let Some(name) = index.filter(|n| !n.is_empty()) {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.add_node(&id, &name, line);
                    if let Some(t) = table.filter(|t| !t.is_empty()) {
                        let target = self.resolve(&t);
                        self.edge(&id, &target, "indexes", line);
                    }
                }
            }
            "fb_proc_or_trigger" => {
                let text = read(node, self.source);
                if let Some(c) = P.fb.captures(&text).map_err(crate::Error::from)? {
                    let name = &c[2];
                    let trigger = c[1].to_uppercase() == "TRIGGER";
                    let id = make_id([self.stem.as_str(), name]);
                    let label = if trigger {
                        name.into()
                    } else {
                        format!("{name}()")
                    };
                    self.add_node(&id, &label, line);
                    if trigger
                        && let Some(c) = P.for_table.captures(&text).map_err(crate::Error::from)?
                    {
                        let target = self.resolve(&c[1]);
                        self.edge(&id, &target, "triggers", line);
                    }
                    let mut non_tables: HashSet<String> = [
                        "select", "where", "set", "dual", "null", "true", "false", "first", "skip",
                        "rows", "next", "only", "lateral",
                    ]
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                    for c in captures(&P.cte, &text)? {
                        non_tables.insert(norm_ident(&c[1]));
                    }
                    let mut seen = HashSet::new();
                    for rx in [&P.reads, &P.update] {
                        for c in captures(rx, &text)? {
                            let key = norm_ident(&c[1]);
                            if !non_tables.contains(&key) && seen.insert(key) {
                                let target = self.resolve(&c[1]);
                                self.edge(&id, &target, "reads_from", line);
                            }
                        }
                    }
                }
            }
            _ => (),
        }
        for child in children(node) {
            self.walk(child)?;
        }
        Ok(())
    }
}
pub fn extract_sql(path: &Path, content: Option<&[u8]>) -> Result<Value> {
    let owned;
    let source = if let Some(content) = content {
        content
    } else {
        owned = match super::base::read_source_bytes(path, true) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}));
            }
        };
        &owned[..]
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&graphicle_tree_sitter_sql::LANGUAGE.into()) {
        return Ok(json!({"nodes":[],"edges":[],"error":e.to_string()}));
    }
    let Some(tree) = parser.parse(source, None) else {
        return Ok(json!({"nodes":[],"edges":[],"error":"SQL parse cancelled"}));
    };
    let root = tree.root_node();
    let path = fsutil::path_string(path);
    let file_id = make_id([path.as_str()]);
    let mut extractor = Extractor {
        source,
        path: path.clone(),
        stem: file_stem(Path::new(&path)),
        file_id: file_id.clone(),
        nodes: vec![
            json!({"id":file_id,"label":Path::new(&path).file_name().map(|s|s.to_string_lossy()).unwrap_or_default(),"file_type":"code","source_file":path,"source_location":null}),
        ],
        edges: vec![],
        seen: HashSet::from([file_id]),
        tables: HashMap::new(),
        routines: HashMap::new(),
    };
    extractor.collect_defined(root);
    // Mirror case (PR #3020, adapted): `REFERENCES "public"."users"` binds to an
    // explicit unqualified `CREATE TABLE "users"`. This runs before the secondary
    // bare-name aliases so a `"other"."users"` definition never captures it, and
    // explicit `public.x` definitions are never shadowed.
    let bare: Vec<(String, String)> = extractor
        .tables
        .iter()
        .filter(|(key, _)| !key.contains('.'))
        .map(|(key, id)| (format!("{DEFAULT_SCHEMA}.{key}"), id.clone()))
        .collect();
    for (qualified, id) in bare {
        extractor.tables.entry(qualified).or_insert(id);
    }
    let mut candidates: HashMap<String, Option<String>> = HashMap::new();
    for (key, id) in &extractor.tables {
        if let Some((_, bare)) = key.rsplit_once('.') {
            let value = candidates
                .entry(bare.into())
                .or_insert_with(|| Some(id.clone()));
            if value.as_ref() != Some(id) {
                *value = None;
            }
        }
    }
    for (key, value) in candidates {
        if let Some(id) = value {
            extractor.tables.entry(key).or_insert(id);
        }
    }
    for stmt in children(root) {
        match stmt.kind() {
            "statement" => {
                for child in children(stmt) {
                    extractor.walk(child)?;
                }
            }
            // "block": a bare `BEGIN;` before `DO $$ ... END $$;` closes on the
            // DO body's END, so the DDL before it sits under a block (#3886).
            "transaction"
            | "block"
            | "fb_proc_or_trigger"
            | "set_term"
            | "declare_external_function"
            | "ERROR" => extractor.walk(stmt)?,
            _ => (),
        }
    }
    let mut emitted: HashSet<(String, String)> = extractor
        .edges
        .iter()
        .filter(|e| e["relation"] == "references")
        .map(|e| {
            (
                e["source"].as_str().unwrap().into(),
                e["target"].as_str().unwrap().into(),
            )
        })
        .collect();
    let text = String::from_utf8_lossy(source);
    for c in captures(&P.table, &text)? {
        let Some(table_id) = extractor.tables.get(&norm_ident(&c[1])).cloned() else {
            continue;
        };
        let pos = c.get(0).unwrap().start();
        let line = text[..pos].bytes().filter(|b| *b == b'\n').count() + 1;
        let tail = &text[pos..];
        let end = P.end.find(&tail[1..]).map_err(crate::Error::from)?;
        let block = if let Some(end) = end {
            &tail[..end.start() + 1]
        } else {
            tail
        };
        for c in captures(&P.references, block)? {
            let target = extractor.resolve(&c[1]);
            if emitted.insert((table_id.clone(), target.clone())) {
                extractor.edge(&table_id, &target, "references", line);
            }
        }
    }
    if root.has_error() {
        let (masked, spans) = scan_sql(&text);
        for c in captures(&P.routines, &masked)? {
            let pos = c.get(0).unwrap().start();
            let character = masked[..pos].chars().count();
            if spans.iter().any(|(s, e)| *s <= character && character < *e) {
                continue;
            }
            let name = &c[1];
            let line = masked[..pos].bytes().filter(|b| *b == b'\n').count() + 1;
            let id = make_id([extractor.stem.as_str(), name]);
            extractor.add_node(&id, &format!("{name}()"), line);
        }
        // `_TRIGGER_RECOVERY_RX` (0.9.71): a trigger whose procedural body
        // broke the parse is recovered with the table it fires ON, unless a
        // cleanly parsed trigger already owns the id.
        for c in captures(&P.triggers, &masked)? {
            let pos = c.get(0).unwrap().start();
            let character = masked[..pos].chars().count();
            if spans.iter().any(|(s, e)| *s <= character && character < *e) {
                continue;
            }
            let name = c[1].to_string();
            let id = make_id([extractor.stem.as_str(), name.as_str()]);
            if extractor.seen.contains(&id) {
                continue;
            }
            let line = masked[..pos].bytes().filter(|b| *b == b'\n').count() + 1;
            extractor.add_node(&id, &name, line);
            let target = extractor.resolve(&c[2]);
            extractor.edge(&id, &target, "triggers", line);
        }
    }
    Ok(json!({"nodes":extractor.nodes,"edges":extractor.edges}))
}
#[cfg(test)]
mod tests {
    fn extract(sql: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema.sql");
        std::fs::write(&path, sql).unwrap();
        super::extract_sql(&path, None).unwrap()
    }
    fn references(result: &serde_json::Value) -> Vec<(String, String)> {
        result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "references")
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                )
            })
            .collect()
    }

    #[test]
    fn default_schema_qualified_reference_binds_to_unqualified_table() {
        let result = extract(
            "CREATE TABLE \"companies\"(id int primary key);\nCREATE TABLE \"users\"(id int, company_id int, FOREIGN KEY (company_id) REFERENCES \"public\".\"companies\"(\"id\"));\n",
        );
        let ids: Vec<&str> = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert!(
            !ids.iter().any(|id| id.ends_with("_public_companies")),
            "{ids:?}"
        );
        let edges = references(&result);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert!(
            edges[0].0.ends_with("_users") && edges[0].1.ends_with("_companies"),
            "{edges:?}"
        );
        // A different schema still refuses to bind and keeps its stub.
        let other = extract(
            "CREATE TABLE \"companies\"(id int primary key);\nCREATE TABLE \"users\"(id int, company_id int, FOREIGN KEY (company_id) REFERENCES \"other\".\"companies\"(\"id\"));\n",
        );
        let edges = references(&other);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].1, "other_companies", "{edges:?}");
        // A bare alias of a definition in another schema is not an unqualified
        // definition: `"public"."order"` keeps its stub instead of binding to
        // `"space name"."order"` (pg_introspect view DDL).
        let aliased = extract(
            "CREATE TABLE \"space name\".\"order\" (id INT);\nCREATE VIEW \"space name\".\"View\" AS SELECT * FROM \"public\".\"order\";\n",
        );
        let reads: Vec<(&str, &str)> = aliased["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "reads_from")
            .map(|e| (e["source"].as_str().unwrap(), e["target"].as_str().unwrap()))
            .collect();
        assert_eq!(reads.len(), 1, "{reads:?}");
        assert_eq!(reads[0].1, "public_order", "{reads:?}");
    }

    // Issue #2712 / PR #2716: `[dbo].[Alpha]` used to be labelled `dbo].[Alpha`.
    #[test]
    fn bracket_delimited_tsql_names_keep_their_brackets_verbatim() {
        let result = extract(
            "CREATE TABLE [dbo].[Alpha](id INT);\nCREATE TABLE [dbo].[Beta](a_id INT REFERENCES [dbo].[Alpha](id), g INT REFERENCES [Gamma](id), d INT REFERENCES dbo.[Delta](id));\nCREATE VIEW [dbo].[V] AS SELECT * FROM [dbo].[Alpha];\n",
        );
        let nodes: Vec<(&str, &str)> = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| (n["id"].as_str().unwrap(), n["label"].as_str().unwrap()))
            .collect();
        let labels: Vec<&str> = nodes.iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            [
                "schema.sql",
                "[dbo].[Alpha]",
                "[dbo].[Beta]",
                "[Gamma]",
                "dbo.[Delta]",
                "[dbo].[V]"
            ],
            "{nodes:?}"
        );
        assert!(
            nodes.iter().any(|(id, _)| id.ends_with("_dbo_alpha")),
            "{nodes:?}"
        );
        let edges = references(&result);
        assert_eq!(edges.len(), 3, "{edges:?}");
        assert!(
            edges[0].0.ends_with("_dbo_beta") && edges[0].1.ends_with("_dbo_alpha"),
            "{edges:?}"
        );
        assert_eq!(
            (edges[1].1.as_str(), edges[2].1.as_str()),
            ("gamma", "dbo_delta")
        );
        let reads: Vec<_> = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "reads_from")
            .map(|e| e["target"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(reads.len(), 1, "{reads:?}");
        assert!(reads[0].ends_with("_dbo_alpha"), "{reads:?}");
    }

    #[test]
    fn grammar_matches_the_pinned_python_package() {
        let language: tree_sitter::Language = graphicle_tree_sitter_sql::LANGUAGE.into();
        assert_eq!(language.abi_version(), 15);
        assert_eq!(language.node_kind_count(), 729);
        assert_eq!(language.parse_state_count(), 17329);
    }
}
