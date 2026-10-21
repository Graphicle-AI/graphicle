//! Reuse across engagements: engagement templates and knowledge packs.
//!
//! **Templates** (`memory.template`). A project's reviewed procedures and
//! decisions, each built field by field from an allow-list
//! ([`TEMPLATE_FIELDS`]): kind, title, body and state. Nothing else leaves:
//! no source quote, link, dependency, id, date or author. In the title and
//! body, the memory's own quotes, the sources and symbols it cites, the
//! project's and workspace's names, terms the caller names, URLs, e-mail
//! addresses, credentials, path-like words, and the project's file names and
//! code-shaped symbol names are replaced by placeholders. The template
//! counts each kind of replacement and nothing more.
//!
//! **Knowledge packs** (`memory.import_pack`). Reviewed memories (a
//! `memory.export`, or a template) imported into another project as drafts
//! a member reviews. Nothing of the exported record is copied but its kind,
//! title and body: evidence and links are rebuilt here. Each symbol anchor
//! lands through its logical identity ([`symbols::land`]): the same
//! relpath, qualified name, kind and ordinal, or the only symbol of that
//! qualified name and kind elsewhere (moved). A quote stays evidence only
//! when a current source here holds it: the same source, or the file its
//! anchored symbol moved to, inside that symbol. Everything else is dropped
//! and counted by reason in the draft's `data.pack`.
use crate::{
    Error, Result, memory,
    store::{one, s},
    string,
    symbols::{self, Landed},
};
use regex::{Regex, RegexBuilder};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

/// `memory.template`'s format.
pub const TEMPLATE_FORMAT: &str = "graphicle.template.v1";
/// The kinds of memory a template carries.
pub const TEMPLATE_KINDS: &[&str] = &["procedure", "decision"];
/// The only fields of a template memory.
pub const TEMPLATE_FIELDS: &[&str] = &["kind", "title", "body", "state"];
/// The only top-level fields of a template.
pub const TEMPLATE_KEYS: &[&str] = &["format", "memories", "redactions"];
/// Terms a caller can name for removal.
pub const MAX_TERMS: usize = 200;
/// Memories one pack imports.
pub const MAX_PACK: usize = 500;
/// The kinds of memory a pack imports.
const PACK_KINDS: &[&str] = &["decision", "preference", "procedure", "task_state", "note"];
/// Words of project keys and locations too generic to remove.
const GENERIC: &[&str] = &[
    "local",
    "git",
    "http",
    "https",
    "www",
    "com",
    "org",
    "net",
    "github",
    "gitlab",
    "bitbucket",
    "users",
    "home",
    "src",
    "main",
    "master",
    "repo",
    "repos",
    "project",
    "projects",
    "folder",
    "workspace",
    "code",
    "dev",
    "developer",
    "tmp",
];

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

fn urls() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"(?i)\b[a-z][a-z0-9+.-]*://[^\s<>()\[\]{}`'\x22]+")
}

fn emails() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+")
}

/// Words with a path separator: `src/ledger.py`, `/home/acme`, `C:\work`.
fn paths() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"[\w.@~+:-]*(?:[/\\][\w.@~+-]+)+[/\\]?")
}

/// Code-shaped words: identifiers, dotted or `::` paths, file names.
fn words() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"[A-Za-z_$][\w$-]*(?:(?:\.|::)[A-Za-z_$][\w$-]*)*")
}

/// A word that reads as code rather than prose: an underscore, a separator,
/// a digit, or a capital after the first letter.
fn code_shaped(word: &str) -> bool {
    word.chars().count() >= 4
        && (word.contains(['_', '.', ':', '-', '$'])
            || word.chars().any(|c| c.is_ascii_digit())
            || word.chars().skip(1).any(char::is_uppercase))
}

/// Literal terms (ignoring case when asked), bounded as words where they
/// start or end with a word character.
fn literals(terms: &BTreeSet<String>, insensitive: bool) -> Result<Option<Regex>> {
    let mut terms: Vec<&String> = terms.iter().collect();
    if terms.is_empty() {
        return Ok(None);
    }
    // The longest first, so a longer term is never cut by a shorter one.
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let pattern: Vec<String> = terms
        .iter()
        .map(|term| {
            let mut p = regex::escape(term);
            if word(term.chars().next()) {
                p = format!(r"\b{p}");
            }
            if word(term.chars().last()) {
                p = format!(r"{p}\b");
            }
            p
        })
        .collect();
    RegexBuilder::new(&pattern.join("|"))
        .case_insensitive(insensitive)
        .size_limit(64 << 20)
        .build()
        .map(Some)
        .map_err(|e| Error::bad(format!("Too many terms to remove: {e}")))
}

/// Replacements shared by every memory of one template.
struct Scrubber {
    terms: Option<Regex>,
    names: Option<Regex>,
    /// Code-shaped symbol names (qualified and last part) of the project.
    symbols: HashSet<String>,
    /// File names, and code-shaped file stems, of the project's files.
    files: HashSet<String>,
    counts: BTreeMap<&'static str, u64>,
}

fn replace(text: &str, pattern: &Regex, placeholder: &str, count: &mut u64) -> String {
    pattern
        .replace_all(text, |_: &regex::Captures<'_>| {
            *count += 1;
            placeholder.to_string()
        })
        .into_owned()
}

impl Scrubber {
    fn scrub(&mut self, text: &str, own: &Own) -> String {
        let mut text = text.to_string();
        let mut tally: Vec<(&'static str, u64)> = Vec::new();
        for quote in &own.quotes {
            let found = text.matches(quote.as_str()).count() as u64;
            if found > 0 {
                text = text.replace(quote.as_str(), "[quote]");
                tally.push(("[quote]", found));
            }
        }
        for (pattern, placeholder) in [
            (own.sources.as_ref(), "[source]"),
            (own.symbols.as_ref(), "[symbol]"),
            (self.terms.as_ref(), "[term]"),
            (self.names.as_ref(), "[project]"),
        ] {
            if let Some(pattern) = pattern {
                let mut found = 0;
                text = replace(&text, pattern, placeholder, &mut found);
                tally.push((placeholder, found));
            }
        }
        let before = text.matches("[credential redacted]").count();
        text = crate::operations::redact(&text);
        let after = text.matches("[credential redacted]").count();
        tally.push(("[credential redacted]", after.saturating_sub(before) as u64));
        for (pattern, placeholder) in [
            (urls(), "[url]"),
            (emails(), "[email]"),
            (paths(), "[path]"),
        ] {
            let mut found = 0;
            text = replace(&text, pattern, placeholder, &mut found);
            tally.push((placeholder, found));
        }
        let (mut symbols, mut files) = (0, 0);
        text = words()
            .replace_all(&text, |found: &regex::Captures<'_>| {
                let word = &found[0];
                if self.files.contains(word) {
                    files += 1;
                    return "[path]".to_string();
                }
                let symbol = self.symbols.contains(word)
                    || word
                        .split([':', '.', '-'])
                        .any(|part| self.symbols.contains(part));
                if symbol {
                    symbols += 1;
                    "[symbol]".to_string()
                } else {
                    word.to_string()
                }
            })
            .into_owned();
        tally.push(("[symbol]", symbols));
        tally.push(("[path]", files));
        for (placeholder, n) in tally {
            *self.counts.entry(placeholder).or_default() += n;
        }
        text
    }
}

/// What one memory cites: its quotes, its sources and its linked symbols.
struct Own {
    quotes: Vec<String>,
    sources: Option<Regex>,
    symbols: Option<Regex>,
}

fn own(data: &Value) -> Result<Own> {
    let mut quotes: Vec<String> = data["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| s(c, "quote").trim().to_string())
        .filter(|q| q.chars().count() >= 4)
        .collect();
    quotes.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    quotes.dedup();
    let mut sources = BTreeSet::new();
    for citation in data["evidence"].as_array().into_iter().flatten() {
        let id = s(citation, "id");
        sources.insert(id.to_string());
        if let Some(path) = id.strip_prefix("file:") {
            sources.insert(path.to_string());
        }
    }
    for key in [
        "dependencies",
        "evidence_links",
        "native_sources",
        "symbol_files",
    ] {
        for id in data[key].as_object().into_iter().flat_map(Map::keys) {
            sources.insert(id.clone());
            if let Some(path) = id.strip_prefix("file:") {
                sources.insert(path.to_string());
            }
        }
    }
    let mut names = BTreeSet::new();
    for link in data["symbols"].as_array().into_iter().flatten() {
        for path in [&link["identity"]["relpath"], &link["relpath"]] {
            if let Some(path) = path.as_str().filter(|p| !p.is_empty()) {
                sources.insert(path.to_string());
            }
        }
        if let Some(qualified) = link["identity"]["qualified_name"].as_str() {
            names.insert(qualified.to_string());
            for part in qualified.split(['.', ':']) {
                if part.chars().count() >= 3 {
                    names.insert(part.to_string());
                }
            }
        }
    }
    sources.retain(|t| t.chars().count() >= 3);
    names.retain(|t| t.chars().count() >= 3);
    Ok(Own {
        quotes,
        sources: literals(&sources, false)?,
        symbols: literals(&names, false)?,
    })
}

/// The project's names: its name, its key and the locations its metadata
/// records, with their distinctive parts.
fn project_names(repository: &Value, workspace: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut add = |value: &str, split: bool| {
        let value = value.trim();
        if value.chars().count() >= 3 {
            names.insert(value.to_string());
        }
        if split {
            for part in value.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
                let part = part.trim_end_matches(".git");
                if part.chars().count() >= 4 && !GENERIC.contains(&part.to_lowercase().as_str()) {
                    names.insert(part.to_string());
                }
            }
        }
    };
    add(s(repository, "name"), false);
    add(workspace, false);
    add(s(repository, "project_key"), true);
    // Locations in the project's metadata (folder, remote, URL).
    let mut pending = vec![&repository["metadata"]];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let key = key.to_lowercase();
                    let location = ["root", "remote", "url", "origin", "path", "folder", "name"]
                        .iter()
                        .any(|k| key.contains(k));
                    match value {
                        Value::String(text) if location => add(text, true),
                        Value::Object(_) | Value::Array(_) => pending.push(value),
                        _ => {}
                    }
                }
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    names
}

/// `memory.template`: the reviewed procedures and decisions of a project,
/// with only [`TEMPLATE_FIELDS`] and the replacements described above.
pub fn template(
    db: &Connection,
    workspace: &str,
    repository: &Value,
    documents: &[Value],
    memories: &[Value],
    args: &Value,
) -> Result<Value> {
    let terms: BTreeSet<String> = match args.get("terms") {
        None | Some(Value::Null) => BTreeSet::new(),
        Some(Value::Array(items)) if items.len() <= MAX_TERMS => items
            .iter()
            .map(|t| {
                t.as_str()
                    .filter(|t| !t.trim().is_empty() && t.len() <= 200)
                    .map(|t| t.trim().to_string())
                    .ok_or_else(|| Error::bad("Each term is a string of 1 to 200 bytes"))
            })
            .collect::<Result<_>>()?,
        Some(_) => {
            return Err(Error::bad(format!(
                "terms must be a list of at most {MAX_TERMS} strings"
            )));
        }
    };
    let name = one(db, "SELECT name FROM workspaces WHERE id=?", [workspace])?
        .map(|row| s(&row, "name").to_string())
        .unwrap_or_default();
    let identifier = s(repository, "id");
    let mut symbols = HashSet::new();
    let mut statement = db.prepare_cached(
        "SELECT DISTINCT qualified_name FROM gf_symbol_heads WHERE repository_id=?1",
    )?;
    let names = statement.query_map([identifier], |row| row.get::<_, String>(0))?;
    for qualified in names {
        let qualified = qualified?;
        for part in qualified.split(['.', ':']).filter(|p| code_shaped(p)) {
            symbols.insert(part.to_string());
        }
        if code_shaped(&qualified) {
            symbols.insert(qualified);
        }
    }
    let mut files = HashSet::new();
    for doc in documents {
        if let Some(path) = s(doc, "id").strip_prefix("file:") {
            let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
            if file.contains('.') || code_shaped(file) {
                files.insert(file.to_string());
            }
            let stem = file.split('.').next().unwrap_or(file);
            if code_shaped(stem) {
                files.insert(stem.to_string());
            }
        }
    }
    let mut scrubber = Scrubber {
        terms: literals(&terms, true)?,
        names: literals(&project_names(repository, &name), true)?,
        symbols,
        files,
        counts: BTreeMap::new(),
    };
    let mut out = Vec::new();
    for memory in memories {
        if memory["state"] != "reviewed" || !TEMPLATE_KINDS.contains(&s(memory, "kind")) {
            continue;
        }
        let own = own(&memory["data"])?;
        out.push(json!({
            "kind": s(memory, "kind"),
            "title": scrubber.scrub(s(memory, "title"), &own),
            "body": scrubber.scrub(s(memory, "body"), &own),
            "state": "reviewed",
        }));
    }
    scrubber.counts.retain(|_, n| *n > 0);
    Ok(json!({"format":TEMPLATE_FORMAT,"memories":out,"redactions":scrubber.counts}))
}

fn bump(counts: &mut Value, key: &str) {
    let n = counts[key].as_u64().unwrap_or(0);
    counts[key] = json!(n + 1);
}

/// `memory.import_pack`: import the reviewed memories of `items` as drafts
/// (see the module documentation). Other states are skipped and counted.
pub fn import(
    db: &Connection,
    repository: &str,
    documents: &[Value],
    items: &[Value],
) -> Result<Value> {
    if items.len() > MAX_PACK {
        return Err(Error::bad(format!("Import at most {MAX_PACK} memories")));
    }
    let mut imported = Vec::new();
    let mut skipped = 0;
    let mut totals = json!({"evidence":{"kept":0,"moved":0,"dropped":{}},"anchors":{"landed":0,"moved":0,"dropped":{}}});
    // One lander and one index of the documents for the whole pack, not one
    // per memory.
    let mut lander = symbols::Lander::new(db, repository, documents);
    let by_id: HashMap<&str, &Value> = documents.iter().map(|d| (s(d, "id"), d)).collect();
    for item in items {
        if item["state"] != "reviewed" {
            skipped += 1;
            continue;
        }
        let kind = string(item, "kind", 100)?;
        if !PACK_KINDS.contains(&kind) {
            return Err(Error::bad(
                "Choose decision, preference, procedure, task_state or note",
            ));
        }
        let title = string(item, "title", 500)?;
        let body = string(item, "body", 64000)?;
        let old = match item.get("data") {
            None | Some(Value::Null) => json!({}),
            Some(value @ Value::Object(_)) => value.clone(),
            Some(_) => return Err(Error::bad("Invalid memory data")),
        };
        let mut marks = json!({"evidence":{"kept":0,"moved":0,"dropped":{}},"anchors":{"landed":0,"moved":0,"dropped":{}}});
        // Each symbol identity once, whatever rungs linked it.
        let mut seen = BTreeSet::new();
        let anchors: Vec<Value> = old["symbols"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|link| link["identity"].is_object())
            .map(|link| link["identity"].clone())
            .filter(|identity| seen.insert(identity.to_string()))
            .take(symbols::MAX_LINKS)
            .collect();
        let landings = lander.land(&anchors)?;
        let mut landed: Vec<&Landed> = Vec::new();
        for landing in &landings {
            match landing {
                Ok(found) => {
                    bump(&mut marks["anchors"], "landed");
                    if found.moved {
                        bump(&mut marks["anchors"], "moved");
                    }
                    landed.push(found);
                }
                Err(reason) => bump(&mut marks["anchors"]["dropped"], reason),
            }
        }
        let doc = |id: &str| by_id.get(id).copied();
        let mut kept = Vec::new();
        for citation in old["evidence"].as_array().into_iter().flatten() {
            let (Some(id), Some(quote)) = (citation["id"].as_str(), citation["quote"].as_str())
            else {
                bump(&mut marks["evidence"]["dropped"], "invalid");
                continue;
            };
            if quote.trim().is_empty() || id.len() > 1500 || quote.len() > 12000 {
                bump(&mut marks["evidence"]["dropped"], "invalid");
                continue;
            }
            let here = doc(id);
            if here.is_some_and(|d| s(d, "content").contains(quote)) {
                bump(&mut marks["evidence"], "kept");
                kept.push(json!({"id":id,"quote":quote}));
                continue;
            }
            // The file its anchored symbol moved to, inside that symbol.
            let moved = id.strip_prefix("file:").and_then(|from| {
                landed.iter().find_map(|l| {
                    let to = format!("file:{}", l.row.identity.relpath);
                    let content = s(doc(&to)?, "content");
                    let range = l.row.range(
                        &graphicle_memory::graph::span::line_starts(content),
                        content.len(),
                    );
                    (l.from == from && to != id && content.get(range)?.contains(quote))
                        .then_some(to)
                })
            });
            match moved {
                Some(to) => {
                    bump(&mut marks["evidence"], "kept");
                    bump(&mut marks["evidence"], "moved");
                    kept.push(json!({"id":to,"quote":quote}));
                }
                None if here.is_none() => bump(&mut marks["evidence"]["dropped"], "source_missing"),
                None => bump(&mut marks["evidence"]["dropped"], "quote_missing"),
            }
        }
        let mut data = memory::dependencies(db, repository, &kept, documents)?;
        symbols::link_landed(db, repository, &landed, documents, &mut data)?;
        data["authority"] = json!("unreviewed_pack");
        data["notes"] = json!({});
        for part in ["evidence", "anchors"] {
            for (key, value) in marks[part].as_object().unwrap().clone() {
                match value {
                    Value::Object(reasons) => {
                        for (reason, n) in reasons {
                            let total = totals[part][&key][&reason].as_u64().unwrap_or(0);
                            totals[part][&key][&reason] = json!(total + n.as_u64().unwrap_or(0));
                        }
                    }
                    n => {
                        let total = totals[part][&key].as_u64().unwrap_or(0);
                        totals[part][&key] = json!(total + n.as_u64().unwrap_or(0));
                    }
                }
            }
        }
        data["pack"] = marks;
        imported.push(memory::save(
            db, repository, None, None, kind, title, body, "draft", &data,
        )?);
    }
    let mut result = json!({"imported":imported,"skipped":skipped,"approval_required":true});
    result["evidence"] = totals["evidence"].take();
    result["anchors"] = totals["anchors"].take();
    Ok(result)
}
