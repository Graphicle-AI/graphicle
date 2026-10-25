// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/robot.py
//! Native translation of graphify/extractors/robot.py. The source-model lexer
//! follows Robot Framework 7.5 (Apache-2.0); no Python parser runs in production.
//!
//! Robot Framework 7.5: Copyright 2008-2015 Nokia Networks; Copyright 2016- Robot Framework Foundation. Apache-2.0.
use super::base::{file_stem, make_id};
use crate::{
    Result,
    dedup_strings::{casefold, whitespace},
    fsutil, paths,
};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

const STDLIBS: &[&str] = &[
    "BuiltIn",
    "Collections",
    "DateTime",
    "Dialogs",
    "Easter",
    "OperatingSystem",
    "Process",
    "Remote",
    "Reserved",
    "Screenshot",
    "String",
    "Telnet",
    "XML",
];
const BDD: &[&str] = &["given ", "when ", "then ", "and ", "but "];
static VARIABLES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([^}]*)\}").unwrap());
fn trim(s: &str) -> &str {
    s.trim_matches(whitespace)
}
fn normalized_spaces(s: &str) -> String {
    s.chars()
        .map(|c| if whitespace(c) { ' ' } else { c })
        .collect()
}
pub fn resolve_robot_import(raw: &str, source: &Path) -> Option<PathBuf> {
    let mut anchored = false;
    let mut text = String::new();
    let mut last = 0;
    let parent = source
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for cap in VARIABLES.captures_iter(trim(raw)) {
        let m = cap.get(0).unwrap();
        let name = cap[1].replace([' ', '_'], "").to_lowercase();
        let replacement = match name.as_str() {
            "/" => "/".into(),
            "curdir" => {
                anchored = true;
                parent.to_string_lossy().into_owned()
            }
            "execdir" if !source.is_absolute() => {
                anchored = true;
                ".".into()
            }
            _ => return None,
        };
        text.push_str(&trim(raw)[last..m.start()]);
        text.push_str(&replacement);
        last = m.end();
    }
    text.push_str(&trim(raw)[last..]);
    if text.contains("%{") {
        return None;
    }
    let mut path = PathBuf::from(text);
    if !anchored && !path.is_absolute() {
        path = parent.join(path);
    }
    Some(PathBuf::from(paths::normpath(&path.to_string_lossy())))
}
pub fn kw_id(name: &str) -> String {
    make_id([name.replace([' ', '_'], "").as_str()])
}
pub fn keyword_targets(name: &str) -> Vec<String> {
    let name = trim(name.rsplit('.').next().unwrap_or(name));
    let mut out = vec![kw_id(name)];
    let low = name.to_lowercase();
    for prefix in BDD {
        if low.starts_with(prefix) && name.chars().count() > prefix.len() {
            out.push(kw_id(&name.chars().skip(prefix.len()).collect::<String>()));
            break;
        }
    }
    out
}

#[derive(Clone, Debug)]
pub(crate) struct Statement {
    cells: Vec<String>,
    line: usize,
}
static SPACE_SPLITTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:{}{{2,}}|\t)", fsutil::PY_WHITESPACE)).unwrap());
static PIPE_SPLITTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:\A|{}+)\|(?:{}+|\z)",
        fsutil::PY_WHITESPACE,
        fsutil::PY_WHITESPACE
    ))
    .unwrap()
});
fn cells(line: &str) -> Vec<String> {
    let line = line.trim_end_matches(whitespace);
    if line.starts_with('|') && line.chars().nth(1).is_none_or(whitespace) {
        let mut out = vec![];
        let first = PIPE_SPLITTER
            .find(line)
            .expect("pipe row starts with separator");
        let mut rest = &line[first.end()..];
        // Robot splits the remaining substring repeatedly: its start anchor
        // recognizes adjacent pipes after separator whitespace was consumed.
        while let Some(m) = PIPE_SPLITTER.find(rest) {
            out.push(rest[..m.start()].into());
            rest = &rest[m.end()..];
        }
        out.push(rest.into());
        out
    } else {
        SPACE_SPLITTER.split(line).map(str::to_owned).collect()
    }
}
fn statements(source: &str) -> Vec<Statement> {
    let mut out: Vec<Statement> = vec![];
    for (i, line) in fsutil::split_lines(source.trim_start_matches('\u{feff}')).enumerate() {
        let mut tokens = cells(line);
        let mut has_data = false;
        let mut continuation = None;
        let mut comment = None;
        for (j, s) in tokens.iter().enumerate() {
            let s = if j == 0 {
                s.trim_start_matches(whitespace)
            } else {
                s.as_str()
            };
            if s.starts_with('#') {
                comment = Some(j);
                break;
            }
            if !s.is_empty() && !has_data {
                if s == "..." && continuation.is_none() {
                    continuation = Some(j)
                } else {
                    has_data = true
                }
            }
        }
        if let Some(j) = comment {
            tokens.truncate(j)
        }
        while tokens.last().is_some_and(String::is_empty) {
            tokens.pop();
        }
        if let Some(j) = continuation {
            let extra = if j < tokens.len() {
                tokens.split_off(j + 1)
            } else {
                vec![]
            };
            let extra = if extra.is_empty() {
                vec![String::new()]
            } else {
                extra
            };
            if let Some(previous) = out.last_mut() {
                previous.cells.extend(extra)
            } else {
                out.push(Statement {
                    cells: extra,
                    line: i + 1,
                })
            }
        } else if has_data {
            out.push(Statement {
                cells: tokens,
                line: i + 1,
            })
        }
    }
    out
}

#[derive(Default)]
struct Language {
    headers: BTreeMap<String, String>,
    settings: BTreeMap<String, String>,
}
static LANGUAGES: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../assets/robot/languages.json")).unwrap());
impl Language {
    fn add(&mut self, name: &str) -> bool {
        let key = casefold(name)
            .chars()
            .filter(|c| !whitespace(*c) && *c != '-')
            .collect::<String>();
        let Some(data) = LANGUAGES.get(&key) else {
            return false;
        };
        for (target, key) in [
            (&mut self.headers, "headers"),
            (&mut self.settings, "settings"),
        ] {
            for (k, v) in data[key].as_object().unwrap() {
                target.insert(k.to_lowercase(), v.as_str().unwrap().to_lowercase());
            }
        }
        true
    }
    fn setting(&self, name: &str) -> String {
        let n = normalized_spaces(name).to_lowercase();
        self.settings.get(&n).cloned().unwrap_or(n)
    }
    fn header(&self, s: &str, resource: bool) -> Section {
        let n = normalized_spaces(s).trim_matches(['*', ' ']).to_lowercase();
        let n = self.headers.get(&n).map(String::as_str).unwrap_or(&n);
        match n {
            "settings" | "setting" => Section::Settings,
            "keywords" | "keyword" => Section::Keywords,
            "test cases" | "test case" | "tasks" | "task" if !resource => Section::Tests,
            _ => Section::Ignored,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Settings,
    Tests,
    Keywords,
    Ignored,
}
struct Unit {
    name: String,
    line: usize,
    test: bool,
    body: Vec<Statement>,
}
enum Entry {
    Setting(Statement, String),
    Unit(Unit),
}

fn valid_setting(
    cells: &[String],
    name: &str,
    seen: &mut BTreeSet<String>,
    allowed: &[&str],
) -> bool {
    if !allowed.contains(&name) {
        return false;
    }
    let multi = ["metadata", "library", "resource", "variables"].contains(&name);
    if seen.contains(name) && !multi {
        return false;
    }
    if [
        "resource",
        "test timeout",
        "test template",
        "timeout",
        "template",
        "name",
    ]
    .contains(&name)
        && cells.len() > 2
    {
        return false;
    }
    seen.insert(name.into());
    true
}
pub(crate) fn is_assign(text: &str) -> bool {
    let s = text
        .strip_suffix('=')
        .map(|s| s.trim_end_matches(whitespace))
        .unwrap_or(text);
    let bytes = s.as_bytes();
    if bytes.len() < 4 || !b"$@&".contains(&bytes[0]) || bytes[1] != b'{' {
        return false;
    }
    let close = |start: usize, open: u8, close: u8| -> Option<usize> {
        let mut level = 0;
        let mut escaped = false;
        for (i, b) in bytes.iter().enumerate().skip(start) {
            if escaped {
                escaped = false;
                continue;
            }
            if *b == b'\\' {
                escaped = true;
                continue;
            }
            if *b == open {
                level += 1
            } else if *b == close {
                level -= 1;
                if level == 0 {
                    return Some(i + 1);
                }
            }
        }
        None
    };
    let Some(mut end) = close(1, b'{', b'}') else {
        return false;
    };
    if end == 3 {
        return false;
    }
    while end < bytes.len() && bytes[end] == b'[' {
        let Some(next) = close(end, b'[', b']') else {
            return false;
        };
        end = next;
    }
    end == bytes.len()
}
struct Builder {
    path: PathBuf,
    file: String,
    stem: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen_nodes: BTreeSet<String>,
    seen_edges: BTreeSet<(String, String, String)>,
}
impl Builder {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen_nodes.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self
                .seen_edges
                .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        let mut e = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if relation == "calls" {
            e["context"] = json!("call")
        }
        self.edges.push(e);
    }
    fn calls(&mut self, source: &str, name: &str, line: usize) {
        for target in keyword_targets(name) {
            self.edge(source, &target, "calls", line)
        }
    }
    fn file_setting(&mut self, row: &Statement, name: &str) {
        let mut raw = row.cells.get(1).map(String::as_str).unwrap_or("");
        if name == "library"
            && row.cells.len() == 3
            && ["AS", "WITH NAME"].contains(&normalized_spaces(raw).as_str())
        {
            raw = &row.cells[2]
        }
        let raw = trim(raw);
        if name == "test template" && row.cells.get(1).is_some_and(|s| s.to_uppercase() == "NONE") {
            return;
        }
        if raw.is_empty() {
            return;
        }
        let file = self.file.clone();
        if ["resource", "variables"].contains(&name)
            || name == "library" && (raw.ends_with(".py") || raw.contains(['/', '\\']))
        {
            if let Some(p) = resolve_robot_import(raw, &self.path) {
                self.edge(
                    &file,
                    &make_id([p.to_string_lossy().as_ref()]),
                    "imports",
                    row.line,
                )
            }
        } else if name == "library" {
            if !STDLIBS.contains(&raw) {
                let id = make_id(["robot_library", raw]);
                self.node(&id, raw, row.line);
                self.edge(&file, &id, "imports", row.line)
            }
        } else if [
            "suite setup",
            "suite teardown",
            "test setup",
            "test teardown",
            "test template",
        ]
        .contains(&name)
        {
            // Fixtures retain leading/trailing whitespace in the parser's name.
            self.calls(
                &file,
                row.cells.get(1).map(String::as_str).unwrap_or(""),
                row.line,
            )
        }
    }
    fn inline_call(&mut self, source: &str, cells: &[String], line: usize, template: bool) {
        if template {
            return;
        }
        let Some(name) = cells.iter().find(|s| !is_assign(s)) else {
            return;
        };
        if ![
            "IF", "ELSE IF", "ELSE", "VAR", "GROUP", "RETURN", "CONTINUE", "BREAK",
        ]
        .contains(&normalized_spaces(name).as_str())
        {
            self.calls(source, name, line)
        }
    }
    fn body(&mut self, unit: Unit, languages: &Language, parent_template: bool) {
        if unit.name.is_empty() {
            return;
        }
        let id = if unit.test {
            make_id([self.stem.as_str(), unit.name.as_str()])
        } else {
            kw_id(&unit.name)
        };
        let file = self.file.clone();
        self.node(&id, &unit.name, unit.line);
        self.edge(&file, &id, "contains", unit.line);
        let mut valid = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut template = parent_template && unit.test;
        let mut depth = 0;
        for (i, row) in unit.body.iter().enumerate() {
            let name = row.cells.first().map(String::as_str).unwrap_or("");
            if depth == 0 && name.starts_with('[') && name.ends_with(']') {
                let name = languages.setting(trim(&name[1..name.len() - 1]));
                let allowed = if unit.test {
                    &[
                        "documentation",
                        "metadata",
                        "tags",
                        "setup",
                        "teardown",
                        "template",
                        "timeout",
                    ][..]
                } else {
                    &[
                        "documentation",
                        "arguments",
                        "setup",
                        "teardown",
                        "timeout",
                        "tags",
                        "return",
                    ][..]
                };
                if valid_setting(&row.cells, &name, &mut seen, allowed) {
                    if name == "template" {
                        let v = row.cells.get(1).map(String::as_str).unwrap_or("");
                        template = !v.is_empty() && v.to_uppercase() != "NONE";
                    }
                    valid.insert(i, name);
                }
            } else if ["FOR", "TRY", "WHILE", "GROUP"].contains(&name)
                || (name == "IF" && row.cells.len() <= 2)
            {
                depth += 1
            } else if name == "END" {
                depth = (depth - 1).max(0)
            }
        }
        let mut depth = 0;
        for (i, row) in unit.body.iter().enumerate() {
            let Some(first) = row.cells.first() else {
                continue;
            };
            if depth == 0 && first.starts_with('[') && first.ends_with(']') {
                if let Some(name) = valid.get(&i)
                    && ["setup", "teardown", "template"].contains(&name.as_str())
                    && let Some(value) = row.cells.get(1).filter(|s| {
                        !s.is_empty() && (name != "template" || s.to_uppercase() != "NONE")
                    })
                {
                    self.calls(&id, value, row.line)
                }
                continue;
            }
            if ["FOR", "TRY", "WHILE", "GROUP"].contains(&first.as_str())
                || (first == "IF" && row.cells.len() <= 2)
            {
                depth += 1;
                continue;
            }
            if first == "END" {
                depth = (depth - 1).max(0);
                continue;
            }
            if [
                "ELSE", "ELSE IF", "EXCEPT", "FINALLY", "BREAK", "CONTINUE", "RETURN", "VAR",
            ]
            .contains(&normalized_spaces(first).as_str())
            {
                continue;
            }
            let if_pos = row.cells.iter().position(|s| !is_assign(s));
            if row.cells.len() > 2 && if_pos.is_some_and(|j| row.cells[j] == "IF") {
                let mut current = vec![];
                let mut condition = false;
                for token in &row.cells {
                    if condition {
                        current.clear();
                        condition = false
                    } else if token == "IF" {
                        current.clear();
                        condition = true
                    } else if normalized_spaces(token) == "ELSE IF" {
                        self.inline_call(&id, &current, row.line, template);
                        current.clear();
                        condition = true
                    } else if token == "ELSE" {
                        self.inline_call(&id, &current, row.line, template);
                        current.clear()
                    } else {
                        current.push(token.clone())
                    }
                }
                self.inline_call(&id, &current, row.line, template);
            } else {
                self.inline_call(&id, &row.cells, row.line, template)
            }
        }
    }
    fn finish(self, error: Option<String>) -> Value {
        let mut v = json!({"nodes":self.nodes,"edges":self.edges});
        if let Some(e) = error {
            v["error"] = json!(e)
        }
        v
    }
}
pub fn extract_source(path: &Path, source: &str) -> Value {
    let file = make_id([path.to_string_lossy().as_ref()]);
    let mut b = Builder {
        path: path.to_owned(),
        file: file.clone(),
        stem: file_stem(path),
        nodes: vec![],
        edges: vec![],
        seen_nodes: BTreeSet::new(),
        seen_edges: BTreeSet::new(),
    };
    b.node(
        &file,
        &path.file_name().unwrap_or_default().to_string_lossy(),
        1,
    );
    // The suffix after `.graphifyrc` language overrides (#2961).
    let resource = crate::extract_languages::effective_suffix(path).to_lowercase() == ".resource";
    let rows = statements(source);
    let mut language = Language::default();
    language.add("en");
    let mut section = Section::Ignored;
    let mut before_header = true;
    let mut entries: Vec<Entry> = vec![];
    let mut current: Option<Unit> = None;
    for row in rows {
        let first = row.cells.first().map(String::as_str).unwrap_or("");
        if first.starts_with('*') {
            before_header = false;
            if let Some(unit) = current.take() {
                entries.push(Entry::Unit(unit))
            }
            section = language.header(first, resource);
            continue;
        }
        if section == Section::Ignored && before_header {
            let combined = row.cells.join(" ");
            if let Some((name, value)) = combined.split_once(':')
                && name.to_lowercase() == "language"
            {
                language.add(trim(value));
            }
        } else if section == Section::Settings {
            let mut name = language.setting(first);
            if let Some(rest) = name.strip_prefix("task ") {
                name = format!("test {rest}")
            }
            entries.push(Entry::Setting(row, name));
        } else if section == Section::Tests || section == Section::Keywords {
            let has_name = !first.is_empty();
            let mut cells = row.cells;
            if has_name || current.is_none() {
                if let Some(unit) = current.take() {
                    entries.push(Entry::Unit(unit))
                }
                let name = if cells.is_empty() {
                    String::new()
                } else {
                    cells.remove(0)
                };
                current = Some(Unit {
                    name,
                    line: row.line,
                    test: section == Section::Tests,
                    body: vec![],
                });
            } else {
                while cells.first().is_some_and(String::is_empty) {
                    cells.remove(0);
                }
            }
            if !cells.is_empty() {
                current.as_mut().unwrap().body.push(Statement {
                    cells,
                    line: row.line,
                })
            }
        }
    }
    if let Some(unit) = current {
        entries.push(Entry::Unit(unit))
    }
    let mut valid = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut template = false;
    for (i, entry) in entries.iter().enumerate() {
        if let Entry::Setting(row, name) = entry {
            let allowed = if resource {
                &[
                    "documentation",
                    "keyword tags",
                    "library",
                    "resource",
                    "variables",
                ][..]
            } else {
                &[
                    "documentation",
                    "metadata",
                    "name",
                    "suite setup",
                    "suite teardown",
                    "test setup",
                    "test teardown",
                    "test template",
                    "test timeout",
                    "test tags",
                    "default tags",
                    "keyword tags",
                    "library",
                    "resource",
                    "variables",
                ][..]
            };
            if valid_setting(&row.cells, name, &mut seen, allowed) {
                valid.insert(i);
                if name == "test template" {
                    template = row.cells.get(1).is_some_and(|v| !v.is_empty())
                }
            }
        }
    }
    for (i, entry) in entries.into_iter().enumerate() {
        match entry {
            Entry::Setting(row, name) if valid.contains(&i) => b.file_setting(&row, &name),
            Entry::Unit(unit) => b.body(unit, &language, template),
            _ => {}
        }
    }
    b.finish(None)
}
pub fn extract_robot(path: &Path) -> Value {
    match fs::read_to_string(path) {
        Ok(source) => extract_source(path, &source),
        Err(e) => {
            let mut r = extract_source(path, "");
            r["error"] = json!(format!("Reading file '{}' failed: {e}", path.display()));
            r
        }
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |k: &str| r[k].as_str().unwrap_or("");
    Ok(match s("function") {
        "_kw_id" => json!(kw_id(s("name"))),
        "_resolve_robot_import" => {
            json!(resolve_robot_import(s("raw"), Path::new(s("source_path"))))
        }
        "keyword_targets" => json!(keyword_targets(s("name"))),
        "is_assign" => json!(is_assign(s("text"))),
        "extract_source" => extract_source(Path::new(s("path")), s("source")),
        "extract_robot" => extract_robot(Path::new(s("path"))),
        _ => return Err("Unknown Robot extractor operation".into()),
    })
}
