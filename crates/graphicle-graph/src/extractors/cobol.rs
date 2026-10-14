// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/cobol.py
//! Port of graphify/extractors/cobol.py as of Graphify 0.9.69 (4139885a).
//!
//! Deterministic regex extraction for fixed- and free-format COBOL source,
//! including sequence-numbered fixed-format files (#3813) and both endpoints
//! of `PERFORM A THRU Z` (#3814).
use super::base::make_id;
use crate::{fsutil, io_compat, path_syntax};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};

const NAME: &str = r"[A-Z0-9][A-Z0-9-]*";

static PROGRAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)\bPROGRAM-ID\s*\.\s*({NAME})")).unwrap());
static PARAGRAPH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^({NAME})(?:\s+(SECTION))?\s*\.\s*$")).unwrap());
static PERFORM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\bPERFORM\s+({NAME})(?:\s+(?:THRU|THROUGH)\s+({NAME}))?\b"
    ))
    .unwrap()
});
static COPY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)\bCOPY\s+(?:'([^']+)'|"([^"]+)"|({NAME}(?:\.[A-Z0-9-]+)?))"#
    ))
    .unwrap()
});
// Python: `CALL\s+(['"])([A-Z0-9-]+)\1` — the regex crate has no backreferences,
// so both quote styles are spelled out.
static CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bCALL\s+(?:'([A-Z0-9-]+)'|"([A-Z0-9-]+)")"#).unwrap());
static DATA_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^\s*(?:0[1-9]|[1-4][0-9]|66|77|88)\s+({NAME})\b"
    ))
    .unwrap()
});
static FREE_FORMAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)>>\s*SOURCE\s+FORMAT\s+(?:IS\s+)?FREE").unwrap());
// Six blank columns or a six-digit sequence number, then a valid indicator:
// space (code), `*`/`/` (comment), `-` (continuation) or `D` (debug).
static FIXED_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?: {6}|\d{6})[ *\-/dD]").unwrap());
static STRING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"'[^']*'|"[^"]*""#).unwrap());
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"'(?:''|[^'])*'|"(?:""|[^"])*""#).unwrap());
static EXEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bEXEC\s+(?:SQL|CICS)\b").unwrap());

const NOT_PARAGRAPHS: &[&str] = &[
    "IDENTIFICATION",
    "ENVIRONMENT",
    "DATA",
    "PROCEDURE",
    "FILE",
    "WORKING-STORAGE",
    "LOCAL-STORAGE",
    "LINKAGE",
    "CONFIGURATION",
    "INPUT-OUTPUT",
    "STOP",
    "EXIT",
    "GOBACK",
    "CONTINUE",
    "DISPLAY",
    "MOVE",
    "COMPUTE",
    "ADD",
    "SUBTRACT",
    "MULTIPLY",
    "DIVIDE",
    "PERFORM",
    "COPY",
    "CALL",
    "IF",
    "ELSE",
    "END-IF",
    "END-PERFORM",
    "END-EXEC",
];
const PERFORM_MODIFIERS: &[&str] = &["UNTIL", "VARYING", "TIMES", "WITH", "TEST", "FOREVER"];

/// Logical COBOL lines with fixed-format continuations joined.
fn code_lines(source: &str) -> Vec<(usize, String)> {
    let physical: Vec<&str> = fsutil::split_lines(source).collect();
    let forced_free = FREE_FORMAT.is_match(source);
    let fixed_markers = physical
        .iter()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| FIXED_MARKER.is_match(line))
        .count();
    let nonempty = physical
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count();
    let free = forced_free || fixed_markers < (nonempty / 2).max(1);

    let mut logical: Vec<(usize, String)> = Vec::new();
    for (index, line) in physical.iter().enumerate() {
        let number = index + 1;
        if free {
            let code = line.split_once("*>").map_or(*line, |(head, _)| head);
            let code = code.trim_end();
            if code.trim().is_empty() || code.trim_start().starts_with(">>") {
                continue;
            }
            logical.push((number, code.to_owned()));
            continue;
        }

        let chars: Vec<char> = line.chars().collect();
        if chars.len() < 7 {
            continue;
        }
        let indicator = chars[6];
        if indicator == '*' || indicator == '/' {
            continue;
        }
        let body: String = chars[7..chars.len().min(72)].iter().collect();
        let code = body
            .split_once("*>")
            .map_or(body.as_str(), |(head, _)| head);
        let code = code.trim_end();
        if code.trim().is_empty() {
            continue;
        }
        if indicator == '-' && !logical.is_empty() {
            let (prior_number, prior) = logical.pop().unwrap();
            let prior = prior.trim_end();
            let joiner = if prior.ends_with('-') { "" } else { " " };
            logical.push((
                prior_number,
                format!("{prior}{joiner}{}", code.trim_start()),
            ));
        } else {
            logical.push((number, code.to_owned()));
        }
    }
    logical
}

fn mask_strings(code: &str) -> String {
    STRING
        .replace_all(code, |captures: &regex::Captures<'_>| {
            " ".repeat(captures[0].chars().count())
        })
        .into_owned()
}

/// Whether byte `offset` lies inside a COBOL quoted literal.
fn starts_inside_string(code: &str, offset: usize) -> bool {
    QUOTED
        .find_iter(code)
        .any(|found| found.start() <= offset && offset < found.end())
}

struct Extractor {
    source_file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    seen_ids: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
}

impl Extractor {
    fn add_node(
        &mut self,
        nid: &str,
        label: &str,
        line: usize,
        kind: &str,
        callable_node: bool,
    ) -> String {
        if self.seen_ids.insert(nid.to_owned()) {
            let mut item = Map::new();
            item.insert("id".into(), json!(nid));
            item.insert("label".into(), json!(label));
            item.insert("file_type".into(), json!("code"));
            item.insert("source_location".into(), json!(format!("L{line}")));
            item.insert(
                "metadata".into(),
                json!({"language": "cobol", "kind": kind}),
            );
            item.insert("source_file".into(), json!(self.source_file));
            if callable_node {
                item.insert("_callable".into(), Value::Bool(true));
            }
            self.nodes.push(Value::Object(item));
        }
        nid.to_owned()
    }

    fn add_edge(
        &mut self,
        source_id: &str,
        target_id: &str,
        relation: &str,
        line: usize,
        target_file: Option<&str>,
    ) {
        if source_id.is_empty() || target_id.is_empty() || source_id == target_id {
            return;
        }
        let key = (
            source_id.to_owned(),
            target_id.to_owned(),
            relation.to_owned(),
        );
        if !self.seen_edges.insert(key) {
            return;
        }
        let mut edge = Map::new();
        edge.insert("source".into(), json!(source_id));
        edge.insert("target".into(), json!(target_id));
        edge.insert("relation".into(), json!(relation));
        edge.insert("confidence".into(), json!("EXTRACTED"));
        edge.insert("source_file".into(), json!(self.source_file));
        edge.insert("source_location".into(), json!(format!("L{line}")));
        edge.insert("weight".into(), json!(1.0));
        if let Some(target_file) = target_file {
            edge.insert("target_file".into(), json!(target_file));
        }
        self.edges.push(Value::Object(edge));
    }
}

pub fn extract_cobol(path: &Path) -> Value {
    let source = match super::base::read_source_text(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };

    let source_file = fsutil::path_string(path);
    let file_id = make_id([source_file.as_str()]);
    let file_name = {
        let parts = path_syntax::posix_parts(&source_file);
        path_syntax::posix_name(&parts).to_owned()
    };
    let mut ex = Extractor {
        source_file,
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        seen_ids: HashSet::new(),
        seen_edges: HashSet::new(),
    };

    ex.add_node(&file_id, &file_name, 1, "file", false);

    let lines = code_lines(&source);
    let mut current_program = file_id.clone();
    let mut current_scope = file_id.clone();
    let mut in_procedure = false;
    let mut in_exec = false;
    let mut paragraphs: HashMap<(String, String), String> = HashMap::new();
    let mut pending_performs: Vec<(String, String, String, usize)> = Vec::new();

    for (line, code) in &lines {
        let line = *line;
        let upper = code.to_uppercase().trim().to_owned();

        if in_exec {
            if upper.contains("END-EXEC") {
                in_exec = false;
            }
            continue;
        }
        if EXEC.is_match(&upper) {
            if !upper.contains("END-EXEC") {
                in_exec = true;
            }
            continue;
        }

        if let Some(program) = PROGRAM.captures(&upper) {
            let name = program[1].to_uppercase();
            let nid = make_id([file_id.as_str(), "program", name.to_lowercase().as_str()]);
            current_program = ex.add_node(&nid, &name, line, "program", true);
            ex.add_edge(
                &file_id.clone(),
                &current_program.clone(),
                "contains",
                line,
                None,
            );
            current_scope = current_program.clone();
            in_procedure = false;
            continue;
        }

        if let Some(copy) = COPY.captures(code) {
            let written = (1..=3)
                .filter_map(|group| copy.get(group))
                .map(|group| group.as_str())
                .find(|group| !group.is_empty())
                .unwrap_or("");
            let copy_name = written.trim_end_matches('.');
            let copy_parts = path_syntax::posix_parts(copy_name);
            let copy_file = path_syntax::posix_name(&copy_parts).to_owned();
            let copy_path = if path_syntax::suffix(&copy_file).is_empty() {
                format!("{copy_name}.cpy")
            } else {
                copy_name.to_owned()
            };
            let target = path.parent().unwrap_or(Path::new("")).join(&copy_path);
            let target = fsutil::path_string(&target);
            let stem = &copy_file[..copy_file.len() - path_syntax::suffix(&copy_file).len()];
            let reference_id = make_id([
                current_program.as_str(),
                "copybook",
                copy_name.to_lowercase().as_str(),
            ]);
            let reference_id = ex.add_node(
                &reference_id,
                &stem.to_uppercase(),
                line,
                "copybook_reference",
                false,
            );
            ex.add_edge(
                &current_program.clone(),
                &reference_id,
                "contains",
                line,
                None,
            );
            let target_id = make_id([target.as_str()]);
            ex.add_edge(
                &current_program.clone(),
                &target_id,
                "imports_from",
                line,
                Some(&target),
            );
        }

        if upper.contains("PROCEDURE DIVISION") {
            in_procedure = true;
            current_scope = current_program.clone();
            continue;
        }

        if !in_procedure {
            if let Some(item) = DATA_ITEM.captures(&upper) {
                let name = item[1].to_uppercase();
                if name != "FILLER" {
                    let item_id = make_id([
                        current_program.as_str(),
                        "data",
                        name.to_lowercase().as_str(),
                    ]);
                    let item_id = ex.add_node(&item_id, &name, line, "data_item", false);
                    ex.add_edge(&current_program.clone(), &item_id, "contains", line, None);
                }
            }
            continue;
        }

        if let Some(paragraph) = PARAGRAPH.captures(&upper)
            && !NOT_PARAGRAPHS.contains(&&paragraph[1])
        {
            let name = paragraph[1].to_uppercase();
            let key = (current_program.clone(), name.to_lowercase());
            let paragraph_id = match paragraphs.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    let nid = make_id([
                        current_program.as_str(),
                        "paragraph",
                        name.to_lowercase().as_str(),
                    ]);
                    let kind = if paragraph.get(2).is_some() {
                        "section"
                    } else {
                        "paragraph"
                    };
                    let nid = ex.add_node(&nid, &name, line, kind, true);
                    paragraphs.insert(key, nid.clone());
                    ex.add_edge(&current_program.clone(), &nid, "contains", line, None);
                    nid
                }
            };
            current_scope = paragraph_id;
            continue;
        }

        let masked = mask_strings(&upper);
        for found in PERFORM.captures_iter(&masked) {
            // `PERFORM A THRU Z` performs both endpoints of the range.
            for group in [found.get(1), found.get(2)].into_iter().flatten() {
                let name = group.as_str().to_uppercase();
                if !PERFORM_MODIFIERS.contains(&name.as_str()) {
                    pending_performs.push((
                        current_scope.clone(),
                        current_program.clone(),
                        name,
                        line,
                    ));
                }
            }
        }

        for found in CALL.captures_iter(&upper) {
            let whole = found.get(0).unwrap();
            if starts_inside_string(&upper, whole.start()) {
                continue;
            }
            let callee = found
                .get(1)
                .or_else(|| found.get(2))
                .map(|group| group.as_str().to_uppercase())
                .unwrap_or_default();
            ex.raw_calls.push(json!({
                "caller_nid": current_scope,
                "callee": callee,
                "language": "cobol",
                "source_file": ex.source_file,
                "source_location": format!("L{line}"),
            }));
        }
    }

    for (caller_id, program_id, name, line) in pending_performs {
        if let Some(target_id) = paragraphs.get(&(program_id, name.to_lowercase())) {
            let target_id = target_id.clone();
            ex.add_edge(&caller_id, &target_id, "calls", line, None);
        }
    }

    let seen_ids = &ex.seen_ids;
    let clean_edges: Vec<Value> = ex
        .edges
        .iter()
        .filter(|edge| {
            let source = edge["source"].as_str().unwrap_or("");
            let target = edge["target"].as_str().unwrap_or("");
            seen_ids.contains(source)
                && (seen_ids.contains(target) || edge["relation"] == "imports_from")
        })
        .cloned()
        .collect();
    json!({"nodes": ex.nodes, "edges": clean_edges, "raw_calls": ex.raw_calls})
}
