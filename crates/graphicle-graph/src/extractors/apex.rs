// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/apex.py
//! Complete translation of Graphify's regex-based Salesforce Apex extractor.
use super::base::{file_stem, make_id};
use crate::{Result, fsutil, path_syntax, unicode15::WORD_RANGES};
use fancy_regex::Regex;
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::LazyLock};

fn compile(pattern: &str) -> Regex {
    // Python IGNORECASE includes dotted/dotless I along with the ordinary
    // Unicode simple folds. These static patterns contain ASCII literals only.
    let mut folded = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            folded.push(c);
            if let Some(next) = chars.next() {
                folded.push(next);
            }
        } else if c.is_ascii_alphabetic() {
            match c.to_ascii_lowercase() {
                'i' => folded.push_str("[iIİı]"),
                'k' => folded.push_str("[kKK]"),
                's' => folded.push_str("[sSſ]"),
                lower => folded.push_str(&format!("[{lower}{}]", lower.to_ascii_uppercase())),
            }
        } else {
            folded.push(c);
        }
    }
    let mut word = String::from("[");
    for (a, b) in WORD_RANGES {
        word.push_str(&format!(r"\u{{{a:x}}}-\u{{{b:x}}}"));
    }
    word.push(']');
    Regex::new(
        &folded
            .replace(r"\w", &word)
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .expect("verified static Apex expression")
}
struct Patterns {
    class: Regex,
    interface: Regex,
    enumeration: Regex,
    trigger: Regex,
    method: Regex,
    annotation: Regex,
    soql: Regex,
    dml: Regex,
}
static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| {
    let access = r"(?:public|private|protected|global|webService)?";
    let sharing = r"(?:\s+(?:with|without|inherited)\s+sharing)?";
    let modifier = r"(?:\s+(?:abstract|virtual|override|static|final|transient|testMethod))?";
    let annotation = r"(?:\s*@\w+(?:\s*\([^)]*\))?\s*)*";
    // A type may be namespace-qualified and carry generic arguments with commas;
    // whitespace is only allowed adjacent to `<`, `>` or `,` (upstream PR #3278).
    let ty = r"[\w.\[\]]+(?:\s*[<>,]\s*[\w.\[\]]*)*";
    Patterns {
        class: compile(&format!(
            r"^{annotation}\s*{access}{sharing}{modifier}\s*class\s+(\w+)(?:\s+extends\s+({ty}))?(?:\s+implements\s+({ty}))?\s*\{{?"
        )),
        interface: compile(&format!(
            r"^{annotation}\s*{access}{sharing}{modifier}\s*interface\s+(\w+)(?:\s+extends\s+({ty}))?\s*\{{?"
        )),
        enumeration: compile(&format!(
            r"^{annotation}\s*{access}{sharing}{modifier}\s*enum\s+(\w+)\s*\{{?"
        )),
        trigger: compile(r"^\s*trigger\s+(\w+)\s+on\s+(\w+)\s*\("),
        method: compile(&format!(
            r"^{annotation}\s*{access}{modifier}\s*(?:static\s+)?{ty}\s+(\w+)\s*\([^)]*\)\s*(?:throws\s+\w+\s*)?\{{?"
        )),
        annotation: compile(r"@(\w+)"),
        soql: compile(r"\[\s*SELECT(?!\w)[^\]]+FROM\s+(\w+)"),
        dml: compile(r"(?<!\w)(insert|update|delete|upsert|merge|undelete)\s+\w"),
    }
});
const CONTROL: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "try",
    "catch",
    "finally",
    "return",
    "throw",
    "new",
    "void",
    "null",
    "true",
    "false",
    "this",
    "super",
    "class",
    "interface",
    "enum",
    "trigger",
    "on",
];
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
/// Split a heritage clause on top-level commas, drop generic arguments and keep
/// the tail segment of a qualified name: `Database.Batchable<sObject>, Schedulable`
/// names `Batchable` and `Schedulable` (upstream PR #3278).
fn heritage_names(clause: &str) -> Vec<&str> {
    let mut parts = vec![];
    let mut start = 0;
    let mut depth = 0usize;
    for (index, ch) in clause.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&clause[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&clause[start..]);
    parts
        .into_iter()
        .filter_map(|raw| {
            let name = trim(raw.split('<').next().unwrap_or(""));
            let name = trim(name.rsplit('.').next().unwrap_or(""));
            (!name.is_empty()).then_some(name)
        })
        .collect()
}
struct Output {
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
}
impl Output {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize, inferred: bool) {
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":if inferred{"INFERRED"}else{"EXTRACTED"},"source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn linked_type(&mut self, owner: &str, name: &str, stem: &str, relation: &str, line: usize) {
        let mut id = make_id([stem, name]);
        if !self.seen.contains(&id) {
            id = make_id([name]);
        }
        self.node(&id, name, line);
        self.edge(owner, &id, relation, line, true);
    }
}
pub fn extract_apex(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_text(path, true) {
        Ok(s) => s,
        Err(_) => return Ok(json!({"nodes":[],"edges":[]})),
    };
    let path_text = fsutil::path_string(path);
    let parts = path_syntax::posix_parts(&path_text);
    let name = path_syntax::posix_name(&parts);
    let stem = file_stem(path);
    let file = make_id([path_text.as_str()]);
    let mut out = Output {
        path: path_text,
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
    };
    out.node(&file, name, 1);
    let mut current: Option<String> = None;
    let mut pending = vec![];
    let p = &*PATTERNS;
    for (index, line) in fsutil::split_lines(&source).enumerate() {
        let number = index + 1;
        let stripped = trim(line);
        if stripped.starts_with('@') {
            for captured in p.annotation.captures_iter(stripped) {
                let captured = captured.map_err(crate::Error::from)?;
                pending.push(captured[1].to_lowercase());
            }
            continue;
        }
        if let Some(captured) = p.trigger.captures(stripped).map_err(crate::Error::from)? {
            let name = &captured[1];
            let object = &captured[2];
            let id = make_id([stem.as_str(), name]);
            out.node(&id, name, number);
            out.edge(&file, &id, "contains", number, false);
            let target = make_id([object]);
            out.node(&target, object, number);
            out.edge(&id, &target, "uses", number, true);
            current = Some(id);
            pending.clear();
            continue;
        }
        if let Some(captured) = p.class.captures(stripped).map_err(crate::Error::from)? {
            let name = &captured[1];
            if CONTROL.contains(&name.to_lowercase().as_str()) {
                pending.clear();
                continue;
            }
            let id = make_id([stem.as_str(), name]);
            out.node(&id, name, number);
            out.edge(&file, &id, "contains", number, false);
            if let Some(bases) = captured.get(2) {
                for base in heritage_names(bases.as_str()) {
                    out.linked_type(&id, base, &stem, "extends", number);
                }
            }
            if let Some(interfaces) = captured.get(3) {
                for interface in heritage_names(interfaces.as_str()) {
                    out.linked_type(&id, interface, &stem, "implements", number);
                }
            }
            current = Some(id);
            pending.clear();
            continue;
        }
        if let Some(captured) = p.interface.captures(stripped).map_err(crate::Error::from)? {
            let name = &captured[1];
            if CONTROL.contains(&name.to_lowercase().as_str()) {
                pending.clear();
                continue;
            }
            let id = make_id([stem.as_str(), name]);
            out.node(&id, name, number);
            out.edge(
                current.as_deref().unwrap_or(&file),
                &id,
                "contains",
                number,
                false,
            );
            if let Some(parents) = captured.get(2) {
                for parent in heritage_names(parents.as_str()) {
                    out.linked_type(&id, parent, &stem, "extends", number);
                }
            }
            pending.clear();
            continue;
        }
        if let Some(captured) = p
            .enumeration
            .captures(stripped)
            .map_err(crate::Error::from)?
        {
            let name = &captured[1];
            if CONTROL.contains(&name.to_lowercase().as_str()) {
                pending.clear();
                continue;
            }
            let id = make_id([stem.as_str(), name]);
            out.node(&id, name, number);
            out.edge(
                current.as_deref().unwrap_or(&file),
                &id,
                "contains",
                number,
                false,
            );
            pending.clear();
            continue;
        }
        if let Some(owner) = &current
            && let Some(captured) = p.method.captures(stripped).map_err(crate::Error::from)?
        {
            let name = &captured[1];
            if !CONTROL.contains(&name.to_lowercase().as_str()) {
                let id = make_id([owner.as_str(), name]);
                out.node(&id, &format!(".{name}()"), number);
                out.edge(owner, &id, "method", number, false);
                if pending
                    .iter()
                    .any(|s| s == "auraenabled" || s == "invocablemethod")
                {
                    out.edge(&file, &id, "contains", number, true);
                }
                pending.clear();
                continue;
            }
        }
        pending.clear();
        for captured in p.soql.captures_iter(line) {
            let captured = captured.map_err(crate::Error::from)?;
            let name = &captured[1];
            let id = make_id([name]);
            out.node(&id, name, number);
            out.edge(
                current
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&file),
                &id,
                "uses",
                number,
                true,
            );
        }
        for captured in p.dml.captures_iter(line) {
            let captured = captured.map_err(crate::Error::from)?;
            let op = captured[1].to_lowercase();
            let id = make_id([format!("dml_{op}").as_str()]);
            out.node(&id, &op, number);
            out.edge(
                current
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&file),
                &id,
                "uses",
                number,
                true,
            );
        }
    }
    Ok(json!({"nodes":out.nodes,"edges":out.edges}))
}

#[cfg(test)]
mod tests {
    use super::{extract_apex, heritage_names};

    #[test]
    fn qualified_and_generic_heritage_and_return_types_are_recognised() {
        assert_eq!(
            heritage_names("Database.Batchable<sObject>, Schedulable"),
            ["Batchable", "Schedulable"]
        );
        assert_eq!(heritage_names("Map<String, Object>"), ["Map"]);
        assert_eq!(heritage_names(" , "), Vec::<&str>::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Batch.cls");
        std::fs::write(
            &path,
            "public class Batch implements Database.Batchable<sObject>, Schedulable {\n    public Database.QueryLocator start(Database.BatchableContext bc) { return null; }\n    public Map<String, Object> mapReturn() { return null; }\n    public void execute(SchedulableContext sc) {}\n}\n",
        )
        .unwrap();
        let result = extract_apex(&path).unwrap();
        let labels: Vec<&str> = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["label"].as_str().unwrap())
            .collect();
        for expected in [
            ".start()",
            ".mapReturn()",
            ".execute()",
            "Batchable",
            "Schedulable",
        ] {
            assert!(
                labels.contains(&expected),
                "{expected} missing from {labels:?}"
            );
        }
        assert!(!labels.contains(&"Database"), "{labels:?}");
        let implemented: Vec<&str> = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "implements")
            .map(|e| e["target"].as_str().unwrap())
            .collect();
        assert_eq!(implemented, ["batchable", "schedulable"]);
    }
}
