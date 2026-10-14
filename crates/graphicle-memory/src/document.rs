//! Hindsight schema-v2 structured documents and eight targeted delta operations.
//! Adaptations: strict bounded input, complete fence lengths, immutable preview.
//! IDs survive replacement; missing targets are reported instead of guessed.
//! An `add_section` anchor may name a section added earlier in the batch, a
//! slug or a heading (Hindsight #4768); a block named under the wrong section
//! is reported with its real section.
use crate::{Result, string};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub id: String,
    pub text: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    pub heading: String,
    pub level: u8,
    pub blocks: Vec<Block>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    #[serde(rename = "version")]
    pub schema_version: u8,
    pub sections: Vec<Section>,
}

fn unique(base: &str, used: &BTreeSet<String>) -> String {
    if !used.contains(base) {
        return base.into();
    }
    let mut suffix = 2;
    while used.contains(&format!("{base}-{suffix}")) {
        suffix += 1;
    }
    format!("{base}-{suffix}")
}
fn slug(heading: &str) -> String {
    static RX: OnceLock<Regex> = OnceLock::new();
    let value = RX
        .get_or_init(|| Regex::new("[^a-z0-9]+").unwrap())
        .replace_all(&heading.trim().to_lowercase(), "-")
        .trim_matches('-')
        .to_owned();
    if value.is_empty() {
        "section".into()
    } else {
        value
    }
}
fn normalized(value: &str) -> String {
    let lines: Vec<_> = value.split('\n').collect();
    let first = lines
        .iter()
        .position(|x| !x.trim().is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|x| !x.trim().is_empty())
        .map_or(first, |x| x + 1);
    lines[first..end].join("\n")
}
fn block(value: &str, used: &mut BTreeSet<String>) -> Block {
    let value = normalized(value);
    // A short stable identifier, not a cryptographic trust check.
    let hash = format!("{:x}", Sha1::digest(value.as_bytes()));
    let id = unique(&format!("b{}", &hash[..8]), used);
    used.insert(id.clone());
    Block { id, text: value }
}
fn fence(line: &str) -> Option<(char, usize, bool)> {
    let text = line.trim_start();
    let ch = text.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = text.chars().take_while(|x| *x == ch).count();
    (count >= 3).then(|| (ch, count, text[count..].trim().is_empty()))
}
pub fn split(markdown: &str) -> Result<Document> {
    if markdown.len() > 1024 * 1024 {
        return Err("Structured document exceeds 1 MiB".into());
    }
    static HEADING: OnceLock<Regex> = OnceLock::new();
    let heading = HEADING.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.*\S)\s*$").unwrap());
    let mut sections = Vec::new();
    let mut current = Section {
        id: "preamble".into(),
        heading: String::new(),
        level: 2,
        blocks: Vec::new(),
    };
    let mut pending = Vec::<String>::new();
    let mut section_ids = BTreeSet::new();
    let mut block_ids = BTreeSet::new();
    let mut opened = None;
    fn flush(pending: &mut Vec<String>, current: &mut Section, ids: &mut BTreeSet<String>) {
        if pending.iter().any(|x| !x.trim().is_empty()) {
            current.blocks.push(block(&pending.join("\n"), ids));
        }
        pending.clear();
    }
    for line in markdown.lines() {
        if let Some((kind, size, empty_tail)) = fence(line) {
            match opened {
                None => opened = Some((kind, size)),
                Some((old, length)) if old == kind && size >= length && empty_tail => opened = None,
                _ => {}
            }
            pending.push(line.into());
            continue;
        }
        if opened.is_none() {
            if let Some(capture) = heading.captures(line) {
                flush(&mut pending, &mut current, &mut block_ids);
                if !current.heading.is_empty() || !current.blocks.is_empty() {
                    section_ids.insert(current.id.clone());
                    sections.push(current);
                }
                let name = capture[2].trim().to_owned();
                let id = unique(&slug(&name), &section_ids);
                section_ids.insert(id.clone());
                current = Section {
                    id,
                    heading: name,
                    level: capture[1].len() as u8,
                    blocks: Vec::new(),
                };
                continue;
            }
            if line.trim().is_empty() {
                flush(&mut pending, &mut current, &mut block_ids);
                continue;
            }
        }
        pending.push(line.into());
    }
    flush(&mut pending, &mut current, &mut block_ids);
    if !current.heading.is_empty() || !current.blocks.is_empty() {
        sections.push(current);
    }
    let result = Document {
        schema_version: 2,
        sections,
    };
    validate(&result)?;
    Ok(result)
}
pub fn render(doc: &Document) -> String {
    let sections: Vec<_> = doc
        .sections
        .iter()
        .map(|section| {
            let mut parts = Vec::new();
            if !section.heading.trim().is_empty() {
                parts.push(format!(
                    "{} {}",
                    "#".repeat(section.level as usize),
                    section.heading.trim()
                ));
            }
            parts.extend(
                section
                    .blocks
                    .iter()
                    .filter(|b| !b.text.trim().is_empty())
                    .map(|b| b.text.clone()),
            );
            parts.join("\n\n")
        })
        .filter(|x| !x.is_empty())
        .collect();
    if sections.is_empty() {
        String::new()
    } else {
        sections.join("\n\n") + "\n"
    }
}
fn validate(doc: &Document) -> Result<()> {
    if doc.schema_version != 2 || doc.sections.len() > 500 {
        return Err("Invalid structured document version or size".into());
    }
    let mut sections = BTreeSet::new();
    let mut blocks = BTreeSet::new();
    let mut bytes = 0;
    for section in &doc.sections {
        if section.id.is_empty()
            || section.id.len() > 200
            || !sections.insert(&section.id)
            || !(1..=6).contains(&section.level)
        {
            return Err("Section IDs must be unique and levels between 1 and 6".into());
        }
        bytes += section.heading.len();
        for block in &section.blocks {
            if block.id.is_empty() || block.id.len() > 200 || !blocks.insert(&block.id) {
                return Err("Block IDs must be unique".into());
            }
            bytes += block.text.len();
        }
    }
    if bytes > 1024 * 1024 || blocks.len() > 5000 {
        return Err("Structured document exceeds its bounds".into());
    }
    Ok(())
}
pub fn document(request: &Value) -> Result<Value> {
    let doc = if let Some(markdown) = request.get("markdown").and_then(Value::as_str) {
        split(markdown)?
    } else {
        serde_json::from_value(
            request
                .get("document")
                .cloned()
                .ok_or("Expected document")?,
        )
        .map_err(|_| "Invalid structured document")?
    };
    validate(&doc)?;
    Ok(json!({"markdown":render(&doc),"document":doc,"normalizes_interblock_whitespace":true}))
}

pub fn operation_schema() -> Value {
    let specifications: &[(&str, &[&str], &[&str])] = &[
        ("append_block", &["section_id", "text"], &[]),
        ("insert_block", &["section_id", "text"], &["after_block_id"]),
        ("replace_block", &["section_id", "block_id", "text"], &[]),
        ("remove_block", &["section_id", "block_id"], &[]),
        (
            "add_section",
            &["heading", "blocks"],
            &["after_section_id", "new_id", "level"],
        ),
        ("remove_section", &["section_id"], &[]),
        ("replace_section_blocks", &["section_id", "blocks"], &[]),
        ("rename_section", &["section_id", "new_heading"], &[]),
    ];
    let variants: Vec<_> = specifications.iter().map(|(name, required, optional)| {
        let mut properties = serde_json::Map::new();
        properties.insert("op".into(),json!({"const":name}));
        for field in required.iter().chain(optional.iter()) {
            let rule = match *field {
                "blocks" => json!({"type":"array","maxItems":1000,"items":{"type":"string"}}),
                "level" => json!({"type":"integer","minimum":1,"maximum":6,"default":2}),
                "after_block_id"|"after_section_id"|"new_id" => json!({"type":["string","null"]}),
                _ => json!({"type":"string"}),
            };
            properties.insert((*field).into(),rule);
        }
        let mut required_fields = vec!["op"]; required_fields.extend_from_slice(required);
        json!({"type":"object","properties":properties,"required":required_fields,"additionalProperties":false})
    }).collect();
    json!({"type":"array","maxItems":100,"items":{"oneOf":variants}})
}

fn table_rows(text: &str) -> bool {
    !text.is_empty() && text.lines().all(|line| line.trim_start().starts_with('|'))
}
fn separator(text: &str) -> bool {
    static RX: OnceLock<Regex> = OnceLock::new();
    let rx = RX.get_or_init(|| Regex::new(r"^\s*\|?[\s:]*-{2,}[\s:|\-]*\|?\s*$").unwrap());
    text.lines().any(|line| rx.is_match(line))
}
fn texts(value: &Value) -> Result<Vec<String>> {
    let rows = value.as_array().ok_or("Expected block text array")?;
    if rows.len() > 1000 {
        return Err("Too many new blocks".into());
    }
    rows.iter()
        .map(|x| {
            x.as_str()
                .or_else(|| x.get("text").and_then(Value::as_str))
                .map(str::to_owned)
                .ok_or_else(|| "Invalid new block text".into())
        })
        .collect()
}

/// Hindsight `resolve_section_anchor` (#4768): an exact id wins; then the id
/// assigned to a section added earlier in this batch, the anchor's slug, and
/// a heading equal to the anchor (a section whose id is not its slug).
fn section_anchor(doc: &Document, added: &BTreeMap<String, String>, anchor: &str) -> Option<usize> {
    let index = |id: &str| doc.sections.iter().position(|s| s.id == id);
    index(anchor)
        .or_else(|| added.get(anchor).and_then(|id| index(id)))
        .or_else(|| index(&slug(anchor)))
        .or_else(|| doc.sections.iter().position(|s| s.heading == anchor))
}

pub fn delta(request: &Value) -> Result<Value> {
    let mut doc: Document = serde_json::from_value(
        request
            .get("document")
            .cloned()
            .ok_or("Expected document")?,
    )
    .map_err(|_| "Invalid structured document")?;
    validate(&doc)?;
    let operations = request
        .get("operations")
        .and_then(Value::as_array)
        .ok_or("Expected delta operations")?;
    if operations.len() > 100 {
        return Err("At most 100 document operations per proposal".into());
    }
    let mut reserved: BTreeSet<_> = doc
        .sections
        .iter()
        .flat_map(|s| s.blocks.iter().map(|b| b.id.clone()))
        .collect();
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    // Hindsight #4768: a model sees a new section's heading, not the
    // collision-safe id it is assigned, so later operations in the same
    // batch may name it by heading or by the `new_id` it asked for.
    let mut added: BTreeMap<String, String> = BTreeMap::new();
    for (index, operation) in operations.iter().enumerate() {
        let mut apply = || -> Result<Value> {
            let op = string(operation, "op")?;
            let fields: &[&str] = match op {
                "append_block" => &["op", "section_id", "text"],
                "insert_block" => &["op", "section_id", "after_block_id", "text"],
                "replace_block" => &["op", "section_id", "block_id", "text"],
                "remove_block" => &["op", "section_id", "block_id"],
                "add_section" => &[
                    "op",
                    "after_section_id",
                    "new_id",
                    "heading",
                    "level",
                    "blocks",
                ],
                "remove_section" => &["op", "section_id"],
                "replace_section_blocks" => &["op", "section_id", "blocks"],
                "rename_section" => &["op", "section_id", "new_heading"],
                _ => return Err("Unsupported document operation".into()),
            };
            if operation
                .as_object()
                .ok_or("Invalid operation")?
                .keys()
                .any(|key| !fields.contains(&key.as_str()))
            {
                return Err("Unexpected document operation field".into());
            }
            if op == "add_section" {
                let position = match operation.get("after_section_id").filter(|x| !x.is_null()) {
                    None => doc.sections.len(),
                    Some(anchor) => {
                        let anchor = anchor.as_str().ok_or("Invalid after_section_id")?;
                        section_anchor(&doc, &added, anchor)
                            .ok_or_else(|| format!("unknown after_section_id: {anchor}"))?
                            + 1
                    }
                };
                let heading = string(operation, "heading")?;
                let level = operation.get("level").and_then(Value::as_u64).unwrap_or(2);
                if !(1..=6).contains(&level) {
                    return Err("Invalid section level".into());
                }
                let values = texts(operation.get("blocks").unwrap_or(&json!([])))?;
                let ids = doc.sections.iter().map(|s| s.id.clone()).collect();
                let id = unique(
                    operation
                        .get("new_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&slug(heading)),
                    &ids,
                );
                let blocks = values
                    .iter()
                    .filter(|x| !x.trim().is_empty())
                    .map(|x| block(x, &mut reserved))
                    .collect();
                doc.sections.insert(
                    position,
                    Section {
                        id: id.clone(),
                        heading: heading.into(),
                        level: level as u8,
                        blocks,
                    },
                );
                added.insert(heading.to_owned(), id.clone());
                if let Some(requested) = operation.get("new_id").and_then(Value::as_str) {
                    added.insert(requested.to_owned(), id.clone());
                }
                return Ok(json!({"index":index,"op":op,"assigned_id":id}));
            }
            let section_id = string(operation, "section_id")?;
            let position = doc
                .sections
                .iter()
                .position(|s| s.id == section_id)
                .ok_or_else(|| format!("unknown section_id: {section_id}"))?;
            let target = match op {
                "replace_block" | "remove_block" => Some(string(operation, "block_id")?),
                "insert_block" => operation
                    .get("after_block_id")
                    .filter(|x| !x.is_null())
                    .map(|x| x.as_str().ok_or("Invalid after_block_id"))
                    .transpose()?,
                _ => None,
            };
            let block_position = target
                .map(|id| {
                    doc.sections[position]
                        .blocks
                        .iter()
                        .position(|b| b.id == id)
                        .ok_or_else(|| {
                            // Hindsight: a real block named under the wrong
                            // section is a targeting mistake; say where it is
                            // rather than edit somewhere the model did not ask.
                            match doc
                                .sections
                                .iter()
                                .find(|s| s.blocks.iter().any(|b| b.id == id))
                            {
                                Some(owner) => format!(
                                    "block_id {id} belongs to section {}, not {section_id}",
                                    owner.id
                                ),
                                None => format!("unknown block_id: {id}"),
                            }
                        })
                })
                .transpose()?;
            let section = &mut doc.sections[position];
            match op {
                "append_block" | "insert_block" | "replace_block" => {
                    let value = string(operation, "text")?;
                    if value.trim().is_empty() {
                        return Err("block text is empty".into());
                    }
                    if op == "replace_block" {
                        section.blocks[block_position.unwrap()].text = normalized(value);
                    } else {
                        let at = if op == "append_block" {
                            section.blocks.len()
                        } else {
                            block_position.map_or(0, |x| x + 1)
                        };
                        if at > 0
                            && table_rows(value)
                            && !separator(value)
                            && table_rows(&section.blocks[at - 1].text)
                            && separator(&section.blocks[at - 1].text)
                        {
                            section.blocks[at - 1].text.push('\n');
                            section.blocks[at - 1].text.push_str(&normalized(value));
                            return Ok(
                                json!({"index":index,"op":op,"merged_into_block_id":section.blocks[at-1].id}),
                            );
                        }
                        section.blocks.insert(at, block(value, &mut reserved));
                    }
                }
                "remove_block" => {
                    section.blocks.remove(block_position.unwrap());
                }
                "remove_section" => {
                    doc.sections.remove(position);
                }
                "replace_section_blocks" => {
                    section.blocks = texts(operation.get("blocks").ok_or("Expected blocks")?)?
                        .iter()
                        .filter(|x| !x.trim().is_empty())
                        .map(|x| block(x, &mut reserved))
                        .collect();
                }
                "rename_section" => {
                    section.heading = string(operation, "new_heading")?.into();
                }
                _ => unreachable!(),
            }
            Ok(json!({"index":index,"op":op,"section_id":section_id}))
        };
        match apply() {
            Ok(value) => applied.push(value),
            Err(reason) => skipped.push(json!({"index":index,"reason":reason})),
        }
    }
    validate(&doc)?;
    Ok(
        json!({"document":doc,"markdown":render(&doc),"changed":!applied.is_empty(),
        "applied":applied,"skipped":skipped,"automatic_commit":false}),
    )
}
