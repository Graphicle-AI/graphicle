// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/dart.py
//! Native translation of Graphify's Dart and Flutter regex extractor.

#[derive(Clone, Copy)]
pub struct ReferencePattern<'a> {
    pub pattern: &'a Regex,
    pub relation: &'a str,
    pub context: &'a str,
    pub blacklist: &'a [&'a str],
}
use super::base::{file_stem, make_id};
use crate::{Result, fsutil, path_syntax, unicode15::WORD_RANGES};
use fancy_regex::Regex;
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::LazyLock};

const BASIC: &[&str] = &[
    "String", "int", "double", "bool", "num", "dynamic", "Object", "void",
];
const STATE: &[&str] = &["String", "List", "Map", "Set", "Future", "Stream", "Object"];
const VARIABLE: &[&str] = &[
    "String", "int", "double", "bool", "num", "dynamic", "Object", "List", "Map", "Set", "void",
];
const FLOW: &[&str] = &["if", "for", "while", "switch", "catch", "return"];
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn trim_start(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn prefix(s: &str, count: usize) -> &str {
    &s[..s
        .char_indices()
        .nth(count)
        .map(|(i, _)| i)
        .unwrap_or(s.len())]
}
/// Python's `\w` as the inside of a bracketed class, for patterns that put it
/// in a class with other characters (`[\w.]`).
fn word_chars() -> String {
    WORD_RANGES
        .iter()
        .map(|(a, b)| format!(r"\u{{{a:x}}}-\u{{{b:x}}}"))
        .collect()
}
fn compile(pattern: &str) -> Regex {
    let word = format!(
        "[{}]",
        WORD_RANGES
            .iter()
            .map(|(a, b)| format!(r"\u{{{a:x}}}-\u{{{b:x}}}"))
            .collect::<String>()
    );
    let boundary = r"(?:(?<!\w)(?=\w)|(?<=\w)(?!\w))";
    Regex::new(
        &pattern
            .replace(r"[\s\S]", "(?s:.)")
            .replace(r"\b", boundary)
            .replace(r"\w", &word)
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .expect("verified static Dart expression")
}
// A directive may carry configurable URIs picking a platform library:
// `export 'x_stub.dart' if (dart.library.io) 'x_io.dart';` (Graphify e402b87).
// A string literal inside the condition is part of the test, not a URI.
const DIRECTIVE_QUOTED: &str = r#"(?:'[^']*'|"[^"]*")"#;
const DIRECTIVE_CONDITION: &str = concat!(
    r#"if\s*\(\s*[\w.]+\s*(?:==\s*"#,
    r#"(?:'[^']*'|"[^"]*")"#,
    r#"\s*)?\)\s*"#
);
fn directive(kind: &str) -> String {
    format!(
        r#"(?m)^\s*{kind}\s+(?P<quote>['"])(?P<uri>[^'"]+)\k<quote>(?P<branches>(?:\s*{DIRECTIVE_CONDITION}{DIRECTIVE_QUOTED})*)"#
    )
}
struct Patterns {
    comment: Regex,
    part: Regex,
    class: Regex,
    extends: Regex,
    with: Regex,
    implements: Regex,
    event: Regex,
    state: Regex,
    add: Regex,
    provider: Regex,
    widget: Regex,
    lookup: Regex,
    annotation: Regex,
    function_annotation: Regex,
    typedef: Regex,
    legacy_typedef: Regex,
    typedef_line: Regex,
    typedef_target: Regex,
    extension: Regex,
    variable: Regex,
    variable_modifier: Regex,
    identifier: Regex,
    method: Regex,
    route_path: Regex,
    route_const: Regex,
    route_object: Regex,
    import: Regex,
    export: Regex,
    import_branch: Regex,
    generic: Regex,
}
static P: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    comment: compile(
        r#"\"\"\"(?:\\.|[\s\S])*?\"\"\"|'''(?:\\.|[\s\S])*?'''|\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|/\*[\s\S]*?\*/|//[^\n]*"#,
    ),
    part: compile(r#"(?m)^\s*part\s+of\s+['\"]([^'\"]+)['\"]"#),
    class: compile(
        r"(?m)^\s*(?:(?:abstract|sealed|base|interface|final|mixin)\s+)*(?:class|mixin|enum|extension\s+type)\s+(\w+)",
    ),
    extends: compile(r"^\s*(?:extends|on)\s+([a-zA-Z0-9_.]+)"),
    with: compile(r"^\s*with\s+"),
    implements: compile(r"^\s*implements\s+"),
    event: compile(r"\bon<(\w+)>\s*\("),
    state: compile(r"\b(?:emit|yield)\s*\(?\s*(?:const\s+)?([A-Z]\w*)\b"),
    add: compile(r"\b(?:\w*[Bb]loc\w*|context\.read<\w+>\(\))\.add\(\s*(?:const\s+)?([A-Z]\w*)\b"),
    provider: compile(r"\bref\.(?:watch|read|listen)\s*\(\s*(\w+)\b"),
    widget: compile(
        r"\bBloc(?:Builder|Listener|Consumer|Provider|Selector)\s*<\s*([a-zA-Z0-9_]+)\b",
    ),
    lookup: compile(r"\b(?:read|watch|select|of)\s*<([a-zA-Z0-9_]+)>"),
    annotation: compile(r"@(\w+)(?:\([^)]*\))?"),
    function_annotation: compile(
        r"(?m)^\s*(?:factory\s+|static\s+|async\s+|external\s+|abstract\s+)?(?:\([^)]+\)|[a-zA-Z0-9_<>,.?]+)(?:\s+[a-zA-Z0-9_<>,.?]+){0,3}\s+(\w+)\s*\(",
    ),
    // `typedef Name<T> = <type>;`, whose type may be a function type with
    // parentheses, and the legacy `typedef int Compare(Object a, Object b);`
    // (0.9.93, #4582). Return-type tokens cannot hold whitespace, so a long
    // line that never reaches `(` fails in linear time.
    typedef: compile(r"(?m)^\s*typedef\s+(\w+)\s*(?:<[^>]+>)?\s*=\s*([^;]+);"),
    legacy_typedef: compile(&format!(
        r"(?m)^[ \t]*typedef[ \t]+(?:[{}<>,.?]+[ \t]+)*?(\w+)[ \t]*(?:<[^>\n]+>)?[ \t]*\(",
        word_chars()
    )),
    // `_TYPEDEF_LINE.match(...)`: a match the typedef step declared.
    typedef_line: compile(r"^\s*typedef\b"),
    // `re.fullmatch(r"([\w.]+)\s*(?:<.*>)?\s*\??", target, re.DOTALL)`.
    typedef_target: compile(&format!(r"(?s)^([{}.]+)\s*(?:<.*>)?\s*\??\z", word_chars())),
    extension: compile(r"(?m)^\s{0,4}extension\s+(\w+)?(?:<[^>]+>)?\s+on\s+(\w+)"),
    variable: compile(
        r"(?m)^\s{0,2}(?:late\s+)?(?:(?:final|const|var)\s+)?(?:\([^)]+\)\s+|([a-zA-Z0-9_<>,.?]+(?:\s+[a-zA-Z0-9_<>,.?]+){0,3})\s+)?(?:(\w+)|(?:\w+\s*)?\(([^)]+)\))\s*(?:=|$|;)",
    ),
    variable_modifier: compile(r"^\s*(?:late|final|const|var)\b"),
    identifier: compile(r"^[a-zA-Z_]\w*$"),
    method: compile(
        r"(?m)^\s{0,2}(?:factory\s+|static\s+|async\s+|external\s+|abstract\s+)?(?:\([^)]+\)|[a-zA-Z0-9_<>,.?]+)(?:\s+[a-zA-Z0-9_<>,.?]+){0,3}\s+(\w+(?:\.\w+)?)\s*\(",
    ),
    route_path: compile(
        r#"\b(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?['\"]([a-zA-Z0-9_/?=&%-]+)['\"]"#,
    ),
    route_const: compile(
        r"\b(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?([A-Z][a-zA-Z0-9_]*\.[a-zA-Z0-9_]+)",
    ),
    route_object: compile(
        r"\b(?:push|replace)\s*\(\s*(?:context\s*,\s*)?.*?\b([A-Z]\w*(?:Route|Screen|Page))\b",
    ),
    import: compile(&directive("import")),
    export: compile(&directive("export")),
    import_branch: compile(&format!(
        r#"{DIRECTIVE_CONDITION}(?P<quote>['"])(?P<uri>[^'"]+)\k<quote>"#
    )),
    generic: compile(r"\b\w+<([a-zA-Z0-9_.]+(?:<[a-zA-Z0-9_.,\s<>]+>)?)\s*>\s*\("),
});
fn captures<'a>(pattern: &Regex, text: &'a str) -> Result<Vec<fancy_regex::Captures<'a>>> {
    pattern
        .captures_iter(text)
        .map(|r| r.map_err(crate::Error::from))
        .collect()
}
fn find<'a>(pattern: &Regex, text: &'a str) -> Result<Option<fancy_regex::Captures<'a>>> {
    pattern.captures(text).map_err(crate::Error::from)
}
pub fn clean_comments(source: &str) -> Result<String> {
    let mut clean = String::new();
    let mut last = 0;
    for capture in captures(&P.comment, source)? {
        let m = capture.get(0).unwrap();
        clean.push_str(&source[last..m.start()]);
        if m.as_str().starts_with('/') {
            clean.extend(std::iter::repeat_n(
                '\n',
                m.as_str().bytes().filter(|b| *b == b'\n').count(),
            ));
        } else {
            clean.push_str(m.as_str());
        }
        last = m.end();
    }
    clean.push_str(&source[last..]);
    Ok(clean)
}
pub fn split_types(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    let mut depth = 0i64;
    for c in text.chars() {
        match c {
            '<' => {
                depth += 1;
                current.push(c);
            }
            '>' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                out.push(trim(&current).to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(trim(&current).into());
    }
    out.retain(|s| !s.is_empty());
    out
}
pub fn matching_brace(text: &str, start: usize) -> usize {
    let Some(first) = text
        .get(start..)
        .and_then(|s| s.find('{'))
        .map(|p| p + start)
    else {
        return text.len();
    };
    let mut count = 1i64;
    let mut double = false;
    let mut single = false;
    let mut escape = false;
    let mut pos = first + 1;
    while pos < text.len() {
        let c = text[pos..].chars().next().unwrap();
        let next = pos + c.len_utf8();
        if escape {
            escape = false;
            pos = next;
            continue;
        }
        if c == '\\' {
            escape = true;
            pos = next;
            continue;
        }
        let triple = if !single && text[pos..].starts_with("\"\"\"") {
            Some("\"\"\"")
        } else if !double && text[pos..].starts_with("'''") {
            Some("'''")
        } else {
            None
        };
        if let Some(delimiter) = triple {
            pos += 3;
            pos = text[pos..]
                .find(delimiter)
                .map(|p| pos + p + 3)
                .unwrap_or(text.len());
            continue;
        }
        if c == '"' && !single {
            double = !double;
        } else if c == '\'' && !double {
            single = !single;
        } else if !single && !double {
            if c == '{' {
                count += 1;
            } else if c == '}' {
                count -= 1;
                if count == 0 {
                    return next;
                }
            }
        }
        pos = next;
    }
    text.len()
}
fn skip_group(text: &str, open: char, close: char) -> &str {
    if !trim_start(text).starts_with(open) {
        return text;
    }
    let start = text.find(open).unwrap();
    let mut depth = 1;
    let mut end = text.len();
    for (offset, c) in text[start + open.len_utf8()..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
        }
        if depth == 0 {
            end = start + open.len_utf8() + offset + c.len_utf8();
            break;
        }
    }
    &text[end..]
}
struct Output<'a> {
    src: &'a str,
    path: String,
    stem: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    defined: HashSet<String>,
}
impl Output<'_> {
    fn line(&self, offset: usize) -> usize {
        self.src[..offset].bytes().filter(|b| *b == b'\n').count() + 1
    }
    fn node(
        &mut self,
        id: &str,
        label: &str,
        typ: &str,
        source: Option<&str>,
        line: Option<usize>,
    ) {
        if self.defined.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":typ,"source_file":source,"source_location":line.filter(|n|*n>0).map(|n|format!("L{n}"))}));
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
    ) {
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","confidence_score":1.0,"source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn declaration(&mut self, name: &str, line: usize) -> String {
        let id = make_id([self.stem.as_str(), name]);
        self.node(&id, name, "code", Some(&self.path.clone()), Some(line));
        self.edge(&self.file_id.clone(), &id, "defines", None, line);
        id
    }
    fn reference(
        &mut self,
        owner: &str,
        name: &str,
        relation: &str,
        context: Option<&str>,
        line: usize,
    ) {
        let id = make_id([name]);
        self.node(&id, name, "code", None, None);
        self.edge(owner, &id, relation, context, line);
    }
    fn pattern_refs(
        &mut self,
        context: ReferencePattern<'_>,
        body: &str,
        base: usize,
        owner: &str,
    ) -> Result<()> {
        let ReferencePattern {
            pattern,
            relation,
            context,
            blacklist,
        } = context;

        for m in captures(pattern, body)? {
            let name = m.get(1).unwrap();
            if !blacklist.contains(&name.as_str()) {
                self.reference(
                    owner,
                    name.as_str(),
                    relation,
                    Some(context),
                    self.line(base + name.start()),
                );
            }
        }
        Ok(())
    }
    fn body<'a>(&self, src: &'a str, start: usize, arrow: bool) -> Option<(usize, &'a str)> {
        let brace = src[start..].find('{').map(|n| start + n)?;
        if src[start..].find(';').is_some_and(|n| start + n < brace)
            || (arrow && src[start..].find("=>").is_some_and(|n| start + n < brace))
        {
            return None;
        }
        Some((brace, &src[brace..matching_brace(src, start)]))
    }
    fn classes(&mut self) -> Result<()> {
        let src = self.src;
        for m in captures(&P.class, src)? {
            let name = m.get(1).unwrap();
            let line = self.line(name.start());
            let owner = self.declaration(name.as_str(), line);
            let rest = prefix(&src[m.get(0).unwrap().end()..], 500);
            let rest = skip_group(skip_group(rest, '<', '>'), '(', ')');
            let end = rest
                .find('{')
                .or_else(|| rest.find(';'))
                .unwrap_or(rest.len());
            let mut header = &rest[..end];
            let mut base = None;
            let mut generics = None;
            let mut mixins = vec![];
            let mut interfaces = vec![];
            if let Some(m) = find(&P.extends, header)? {
                base = Some(m.get(1).unwrap().as_str());
                let rest = &header[m.get(0).unwrap().end()..];
                header = rest;
                if trim(rest).starts_with('<') {
                    let start = rest.find('<').unwrap();
                    let mut depth = 1;
                    for (offset, c) in rest[start + 1..].char_indices() {
                        if c == '<' {
                            depth += 1;
                        } else if c == '>' {
                            depth -= 1;
                            if depth == 0 {
                                let end = start + 1 + offset;
                                generics = Some(&rest[start + 1..end]);
                                header = &rest[end + 1..];
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(m) = find(&P.with, header)? {
                let rest = &header[m.get(0).unwrap().end()..];
                let mixin = if let Some(i) = rest.find("implements") {
                    header = &rest[i..];
                    &rest[..i]
                } else {
                    header = "";
                    rest
                };
                mixins = split_types(mixin);
            }
            if let Some(m) = find(&P.implements, header)? {
                interfaces = split_types(&header[m.get(0).unwrap().end()..]);
            }
            if let Some(base) = base.filter(|s| !s.is_empty()) {
                self.reference(&owner, base, "inherits", None, line);
                if let Some(generics) = generics.filter(|s| !s.is_empty()) {
                    for generic in split_types(generics) {
                        let name = trim(generic.split('<').next().unwrap());
                        if !BASIC.contains(&name) {
                            self.reference(&owner, name, "references", None, line);
                        }
                    }
                }
            }
            for (types, relation) in [(mixins, "mixes_in"), (interfaces, "implements")] {
                for typ in types {
                    self.reference(
                        &owner,
                        trim(typ.split('<').next().unwrap()),
                        relation,
                        None,
                        line,
                    );
                }
            }
            if let Some((offset, body)) = self.body(src, m.get(0).unwrap().start(), false) {
                for (pattern, relation, context, blacklist) in [
                    (&P.event, "calls", "bloc_event", &[][..]),
                    (&P.state, "calls", "emit_state", STATE),
                    (&P.add, "calls", "bloc_add_event", STATE),
                    (&P.provider, "references", "riverpod_reference", &[][..]),
                    (&P.widget, "references", "bloc_widget_binding", BASIC),
                    (&P.lookup, "references", "bloc_lookup", BASIC),
                ] {
                    self.pattern_refs(
                        ReferencePattern {
                            pattern,
                            relation,
                            context,
                            blacklist,
                        },
                        body,
                        offset,
                        &owner,
                    )?;
                }
            }
        }
        Ok(())
    }
    fn annotations(&mut self) -> Result<()> {
        let src = self.src;
        for annotation in captures(&P.annotation, src)? {
            let name = annotation.get(1).unwrap().as_str();
            if [
                "override",
                "deprecated",
                "required",
                "protected",
                "mustCallSuper",
            ]
            .contains(&name)
            {
                continue;
            }
            let between = prefix(&src[annotation.get(0).unwrap().end()..], 300);
            let class = find(&P.class, between)?;
            let function = find(&P.function_annotation, between)?;
            let class_first = match (&class, &function) {
                (Some(c), Some(f)) => c.get(0).unwrap().start() < f.get(0).unwrap().start(),
                (Some(_), None) => true,
                _ => false,
            };
            let target = if class_first {
                class.as_ref()
            } else {
                function.as_ref()
            };
            let Some(target) = target else {
                continue;
            };
            let target = target.get(1).unwrap().as_str();
            let id = make_id([self.stem.as_str(), target]);
            if id.is_empty() || target.is_empty() {
                continue;
            }
            let end = class
                .as_ref()
                .map(|m| m.get(0).unwrap().start())
                .unwrap_or(between.len())
                .min(
                    function
                        .as_ref()
                        .map(|m| m.get(0).unwrap().start())
                        .unwrap_or(between.len()),
                );
            if between[..end].contains([';', '}', '{']) {
                continue;
            }
            let line = self.line(annotation.get(1).unwrap().start());
            let aid = make_id(["annotation", name.to_lowercase().as_str()]);
            self.node(&aid, &format!("@{name}"), "concept", None, None);
            self.edge(&id, &aid, "configures", None, line);
            if name.to_lowercase() == "riverpod" {
                let provider = if class_first {
                    let first = target.chars().next().unwrap();
                    format!(
                        "{}{}Provider",
                        first.to_lowercase(),
                        &target[first.len_utf8()..]
                    )
                } else {
                    format!("{target}Provider")
                };
                let pid = make_id([provider.as_str()]);
                self.node(
                    &pid,
                    &provider,
                    "concept",
                    Some(&self.path.clone()),
                    Some(line),
                );
                self.edge(&id, &pid, "defines", Some("riverpod_provider"), line);
            }
        }
        Ok(())
    }
    fn types_and_variables(&mut self) -> Result<()> {
        let src = self.src;
        // 2.5 Typedefs: every alias is a node of this file; only an alias of a
        // plain named type references it (a function type's leading name is
        // its return type). Most files declare none: one substring test skips
        // both scans.
        let patterns: &[&Regex] = if src.contains("typedef") {
            &[&P.typedef, &P.legacy_typedef]
        } else {
            &[]
        };
        for (index, pattern) in patterns.iter().enumerate() {
            for m in captures(pattern, src)? {
                let name = m.get(1).unwrap();
                let line = self.line(name.start());
                if self
                    .defined
                    .contains(&make_id([self.stem.as_str(), name.as_str()]))
                {
                    continue;
                }
                let id = self.declaration(name.as_str(), line);
                if index != 0 {
                    continue;
                }
                let target = trim(m.get(2).unwrap().as_str());
                let Some(named) = P.typedef_target.captures(target)? else {
                    continue;
                };
                let typ = named.get(1).unwrap().as_str().rsplit('.').next().unwrap();
                if !typ.is_empty() && !VARIABLE.contains(&typ) && typ != "Function" {
                    self.reference(&id, typ, "references", Some("typedef"), line);
                }
            }
        }
        for m in captures(&P.extension, src)? {
            let target = m.get(2).unwrap();
            let line = self.line(target.start());
            let name = m
                .get(1)
                .map(|n| n.as_str().to_string())
                .unwrap_or_else(|| format!("{}_anonymous_extension", self.stem));
            let label = m
                .get(1)
                .map(|n| n.as_str().to_string())
                .unwrap_or_else(|| format!("Extension on {}", target.as_str()));
            let id = make_id([self.stem.as_str(), name.as_str()]);
            self.node(&id, &label, "code", Some(&self.path.clone()), Some(line));
            self.edge(&self.file_id.clone(), &id, "defines", None, line);
            self.reference(&id, target.as_str(), "extends", None, line);
        }
        for m in captures(&P.variable, src)? {
            let typ = m.get(1).map(|s| s.as_str());
            if find(&P.variable_modifier, m.get(0).unwrap().as_str())?.is_none()
                && typ.is_none_or(|t| t.is_empty())
            {
                continue;
            }
            // Declared as an alias in 2.5 (0.9.93, #4582).
            if find(&P.typedef_line, m.get(0).unwrap().as_str())?.is_some() {
                continue;
            }
            if let Some(name) = m.get(2).filter(|s| !s.as_str().is_empty()) {
                if FLOW.contains(&name.as_str()) {
                    continue;
                }
                let line = self.line(name.start());
                self.declaration(name.as_str(), line);
                if let Some(typ) = typ.filter(|s| !s.is_empty() && !VARIABLE.contains(s)) {
                    let typ = trim(typ.split('<').next().unwrap().rsplit('.').next().unwrap());
                    self.reference(
                        &self.file_id.clone(),
                        typ,
                        "references",
                        Some("variable_type"),
                        line,
                    );
                }
            } else if let Some(names) = m.get(3).filter(|s| !s.as_str().is_empty()) {
                let line = self.line(names.start());
                for name in names
                    .as_str()
                    .split(',')
                    .map(trim)
                    .filter(|n| !n.is_empty())
                {
                    let name = trim(name.rsplit(':').next().unwrap());
                    if find(&P.identifier, name)?.is_some()
                        && !name.starts_with(|c: char| c.is_ascii_uppercase())
                        && !FLOW.contains(&name)
                    {
                        self.declaration(name, line);
                    }
                }
            }
        }
        Ok(())
    }
    fn methods(&mut self) -> Result<()> {
        let src = self.src;
        for m in captures(&P.method, src)? {
            // A legacy alias, declared in 2.5 (0.9.93, #4582).
            if find(&P.typedef_line, m.get(0).unwrap().as_str())?.is_some() {
                continue;
            }
            let raw = m.get(1).unwrap();
            let name = raw.as_str().rsplit('.').next().unwrap();
            if FLOW.contains(&name)
                || ["void", "dynamic", "final", "const", "get", "set"].contains(&name)
                || name.starts_with(|c: char| c.is_ascii_uppercase())
            {
                continue;
            }
            let owner = self.declaration(name, self.line(raw.start()));
            if let Some((offset, body)) = self.body(src, m.get(0).unwrap().start(), true) {
                for (pattern, relation, context, blacklist) in [
                    (&P.provider, "references", "riverpod_reference", &[][..]),
                    (&P.add, "calls", "bloc_add_event", STATE),
                    (&P.lookup, "references", "bloc_lookup", BASIC),
                ] {
                    self.pattern_refs(
                        ReferencePattern {
                            pattern,
                            relation,
                            context,
                            blacklist,
                        },
                        body,
                        offset,
                        &owner,
                    )?;
                }
                for (pattern, context) in [
                    (&P.route_path, "route_path"),
                    (&P.route_const, "route_const"),
                    (&P.route_object, "route_object"),
                ] {
                    for m in captures(pattern, body)? {
                        let name = m.get(1).unwrap();
                        let name_text = name.as_str();
                        let line = self.line(offset + name.start());
                        let (id, label, typ) = match context {
                            "route_path" => (
                                make_id([
                                    "route",
                                    name_text.replace(['/', '?', '=', '&'], "_").as_str(),
                                ]),
                                format!("Route {name_text}"),
                                "concept",
                            ),
                            "route_const" => (
                                make_id(["route", name_text.replace('.', "_").as_str()]),
                                name_text.into(),
                                "concept",
                            ),
                            _ => (make_id([name_text]), name_text.into(), "code"),
                        };
                        self.node(&id, &label, typ, None, None);
                        self.edge(&owner, &id, "navigates", Some(context), line);
                    }
                }
            }
        }
        Ok(())
    }
    fn imports_and_lookups(&mut self) -> Result<()> {
        let src = self.src;
        for (pattern, relation) in [(&P.import, "imports"), (&P.export, "exports")] {
            for m in captures(pattern, src)? {
                let uri = m.name("uri").unwrap();
                let mut uris = vec![(uri.as_str(), uri.start(), None)];
                if let Some(branches) = m.name("branches") {
                    for branch in captures(&P.import_branch, branches.as_str())? {
                        let uri = branch.name("uri").unwrap();
                        uris.push((
                            uri.as_str(),
                            branches.start() + uri.start(),
                            Some("conditional_uri"),
                        ));
                    }
                }
                for (name, offset, context) in uris {
                    self.reference(
                        &self.file_id.clone(),
                        name,
                        relation,
                        context,
                        self.line(offset),
                    );
                }
            }
        }
        for m in captures(&P.generic, src)? {
            let raw = m.get(1).unwrap();
            let typ = trim(raw.as_str().rsplit('.').next().unwrap());
            let clean = trim(typ.split('<').next().unwrap());
            if !VARIABLE.contains(&clean) && !["Future", "Stream"].contains(&clean) {
                self.reference(
                    &self.file_id.clone(),
                    clean,
                    "references",
                    Some("type_lookup"),
                    self.line(raw.start()),
                );
            }
        }
        Ok(())
    }
}
pub fn extract_dart(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_text(path, true) {
        Ok(s) => s,
        Err(_) => return Ok(json!({"error":format!("cannot read {}",fsutil::path_string(path))})),
    };
    let clean = clean_comments(&source)?;
    let mut stem = file_stem(path);
    let path_text = fsutil::path_string(path);
    let mut file_id = make_id([path_text.as_str()]);
    let mut part = false;
    let mut part_of_line = 0;
    if let Some(m) = find(&P.part, &clean)? {
        let reference = m.get(1).unwrap();
        let parent = reference.as_str();
        if parent.ends_with(".dart")
            && let Ok(parent) =
                fsutil::resolve_missing(&path.parent().unwrap_or(Path::new(".")).join(parent))
            && parent.exists()
        {
            stem = file_stem(&parent);
            file_id = make_id([fsutil::path_string(&parent).as_str()]);
            part = true;
            part_of_line = clean[..reference.start()].matches('\n').count() + 1;
        }
    }
    // Every scanned file keeps its own node (Graphify 1dcac12). A part file's
    // symbols still belong to its library (`file_id`); the part's node is
    // linked from the library and to those symbols after extraction.
    let own_id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let nodes = vec![
        json!({"id":own_id,"label":path_syntax::posix_name(&parts),"file_type":"code","source_file":path_text,"source_location":null}),
    ];
    let mut out = Output {
        src: &clean,
        path: path_text,
        stem,
        file_id,
        nodes,
        edges: vec![],
        defined: HashSet::new(),
    };
    out.classes()?;
    out.annotations()?;
    out.types_and_variables()?;
    out.methods()?;
    out.imports_and_lookups()?;
    if part {
        // The library includes this part; the part physically contains the
        // declarations the library defines from it, ids unchanged.
        let contained = out
            .edges
            .iter()
            .filter(|e| e["source"] == out.file_id.as_str() && e["relation"] == "defines")
            .map(|e| {
                let mut edge = e.clone();
                edge["source"] = json!(own_id);
                edge["relation"] = json!("contains");
                edge
            })
            .collect::<Vec<_>>();
        let library = out.file_id.clone();
        out.edge(&library, &own_id, "includes", None, part_of_line);
        out.edges.extend(contained);
    }
    Ok(json!({"nodes":out.nodes,"edges":out.edges}))
}
