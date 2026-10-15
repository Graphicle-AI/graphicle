// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/markdown.py
//! Port of graphify/extractors/markdown.py at the pinned Graphify commit.
use super::{base::file_stem, markdown_data};
use crate::{fsutil, ids::make_id, io_compat, path_syntax, regex_compat, security};
use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};
use unicode_normalization::UnicodeNormalization;

const LINKABLE: &[&str] = &[".md", ".mdx", ".qmd", ".markdown", ".rst", ".txt"];
pub const FRONTMATTER_MAX_LINES: usize = 200;
// A destination is `<bracketed>`, the only CommonMark form that may hold
// spaces (`[t](<My Note.md>)`), tried first and followed only by a title, or a
// bare run without whitespace; callers take whichever group matched (#4178).
static INLINE: LazyLock<FancyRegex> = LazyLock::new(|| {
    fancy(concat!(
        r"(?<!\!)\[[^\]]*\]\(\s*(?:",
        r#"<([^<>\n]+)>\s*(?:"[^"\n]*"|'[^'\n]*'|\([^()\n]*\))?\s*"#,
        r"|<?([^)\s>]+)>?(?:\s+[^)]*)?",
        r")\)",
    ))
});
// Group 1 is the page (empty for a same-page `[[#Heading]]`), group 2 the heading
// fragment; a table-escaped alias pipe `\|` is not part of the page (PRs #3450, #3772).
static WIKI: LazyLock<FancyRegex> =
    LazyLock::new(|| fancy(r"(?<!\!)\[\[([^\]|#]*?)(?:#([^\]|]*))?(?:\\?\|[^\]]*)?\]\]"));
static CODE: LazyLock<FancyRegex> = LazyLock::new(|| fancy(r"(?<!`)(`+)(?!`)(.+?)(?<!`)\1(?!`)"));
static REF: LazyLock<FancyRegex> = LazyLock::new(|| {
    fancy(r#"^\s{0,3}\[[^\]]+\]:\s*(?:<([^<>\n]+)>(?=\s*(?:["'(]|$))|<?([^\s>]+)>?)"#)
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| regex_compat::compile(r"^(#{1,6})\s+(.+)"));
static PATH_MENTION: LazyLock<Regex> = LazyLock::new(|| {
    regex_compat::compile(
        r"^([A-Za-z0-9_./\-]+\.[A-Za-z0-9]+)::([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)(?:\(\))?$",
    )
});
static BARE_MENTION: LazyLock<Regex> =
    LazyLock::new(|| regex_compat::compile(r"^(?:[A-Za-z_]\w*\.)*([A-Za-z_]\w*)(?:\(\))?$"));
static FLAT: LazyLock<Regex> =
    LazyLock::new(|| regex_compat::compile(r"^([A-Za-z0-9_][A-Za-z0-9_\-. ]*):\s*(.*)$"));

fn fancy(pattern: &str) -> FancyRegex {
    FancyRegex::new(&pattern.replace(r"\s", fsutil::PY_WHITESPACE))
        .expect("static Markdown pattern")
}
fn strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

pub fn split_frontmatter<'a>(lines: &'a [&'a str]) -> (&'a [&'a str], usize) {
    if lines.first().is_none_or(|line| strip(line) != "---") {
        return (&[], 0);
    }
    for i in 1..lines.len().min(FRONTMATTER_MAX_LINES + 1) {
        if matches!(strip(lines[i]), "---" | "...") {
            return (&lines[1..i], i + 1);
        }
    }
    (&[], 0)
}
pub fn parse_frontmatter_fallback(lines: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for raw in lines {
        if raw
            .chars()
            .next()
            .is_none_or(|c| strip(&c.to_string()).is_empty())
        {
            continue;
        }
        if let Some(c) = FLAT.captures(strip(raw)) {
            let value = strip(&c[2]);
            if !value.is_empty() {
                out.insert(strip(&c[1]).into(), json!(value.trim_matches(['\'', '"'])));
            }
        }
    }
    out
}
pub fn parse_frontmatter(lines: &[&str]) -> Map<String, Value> {
    if lines.is_empty() {
        return Map::new();
    }
    match super::yaml_frontmatter::parse(&lines.join("\n")) {
        Ok(Value::Object(map)) => map,
        Ok(_) => Map::new(),
        Err(_) => parse_frontmatter_fallback(lines),
    }
}

// Scoped thread-local state replaces the Python extractor's active scan root.
// A guard restores the prior root, including during unwinding and nested scans.
thread_local! { static ACTIVE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }
pub struct ScanRootGuard(Option<PathBuf>);
impl Drop for ScanRootGuard {
    fn drop(&mut self) {
        ACTIVE_ROOT.with(|root| *root.borrow_mut() = self.0.take());
    }
}
pub fn enter_scan_root(root: Option<&Path>) -> ScanRootGuard {
    ScanRootGuard(ACTIVE_ROOT.with(|old| old.replace(root.map(Path::to_path_buf))))
}
pub fn active_scan_root() -> Option<PathBuf> {
    ACTIVE_ROOT.with(|root| root.borrow().clone())
}

pub type LinkIndex = HashMap<String, Vec<(usize, String, PathBuf)>>;
/// Built once per vault root and then only read: lookups share the index
/// rather than holding the lock through each one.
static LINK_CACHE: LazyLock<Mutex<HashMap<String, std::sync::Arc<LinkIndex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub fn clear_link_index_cache() {
    LINK_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}
pub fn nfc(text: &str) -> String {
    text.nfc().collect()
}
pub fn build_link_index(root: &Path) -> crate::Result<LinkIndex> {
    fn walk(
        dir: &Path,
        root: &Path,
        index: &mut LinkIndex,
        ignored: &mut crate::detect_ignore::IgnoreState,
        warnings: &mut crate::detect_ignore::EncodingWarnings,
    ) -> crate::Result<()> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // IgnoreState anchors rules at the canonical scan root. Keep the
            // caller's path spelling in links, but compare the same relative
            // path beneath that canonical root (e.g. macOS /var -> /private/var).
            let ignore_path = ignored.root.join(path.strip_prefix(root).unwrap_or(&path));
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !name.starts_with('.')
                    && !markdown_data::SKIP_DIRS.contains(&name.as_str())
                    && !entry.file_type().is_ok_and(|kind| kind.is_symlink())
                    && !ignored.predicate(&ignore_path, warnings)?
                {
                    dirs.push(path);
                }
            } else if LINKABLE.contains(&path_syntax::suffix(&name).to_lowercase().as_str())
                && !ignored.predicate(&ignore_path, warnings)?
            {
                let rel = fsutil::path_string(path.strip_prefix(root).unwrap_or(&path))
                    .replace('\\', "/");
                index.entry(nfc(&name)).or_default().push((
                    rel.matches('/').count(),
                    nfc(&rel),
                    path,
                ));
            }
        }
        dirs.sort();
        for dir in dirs {
            walk(&dir, root, index, ignored, warnings)?;
        }
        Ok(())
    }
    let mut index = LinkIndex::new();
    let mut warnings = crate::detect_ignore::EncodingWarnings::default();
    let mut ignored = crate::detect_ignore::IgnoreState::new(root, &[], true, &mut warnings)?;
    walk(root, root, &mut index, &mut ignored, &mut warnings)?;
    Ok(index)
}
pub fn vault_lookup(target: &str, root: &Path) -> crate::Result<Option<PathBuf>> {
    let index = {
        let mut cache = LINK_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        match cache.entry(fsutil::path_string(root)) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
            std::collections::hash_map::Entry::Vacant(entry) => entry
                .insert(std::sync::Arc::new(build_link_index(root)?))
                .clone(),
        }
    };
    let target = nfc(&target.replace('\\', "/"));
    let Some(candidates) = index.get(target.rsplit('/').next().unwrap_or_default()) else {
        return Ok(None);
    };
    let nested = format!("/{target}");
    Ok(candidates
        .iter()
        .filter(|(_, rel, _)| rel == &target || rel.ends_with(&nested))
        .min()
        .map(|(_, _, path)| path.clone()))
}

fn normpath(path: &Path) -> PathBuf {
    let mut normalized = Vec::<String>::new();
    for part in path_syntax::posix_parts(&fsutil::path_string(path)) {
        if part == ".." {
            if normalized
                .last()
                .is_some_and(|p| p != ".." && p != "/" && p != "//")
            {
                normalized.pop();
            } else if normalized.first().is_none_or(|p| p != "/" && p != "//") {
                normalized.push(part);
            }
        } else {
            normalized.push(part);
        }
    }
    PathBuf::from(path_syntax::posix_join(&normalized))
}
/// `urllib.parse.unquote`: `%XX` escapes inside each ASCII run become bytes,
/// read as UTF-8 with U+FFFD for invalid sequences; other characters and
/// malformed escapes are kept as written.
fn unquote(text: &str) -> String {
    if !text.contains('%') {
        return text.to_owned();
    }
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let ascii = rest.find(|c: char| !c.is_ascii()).unwrap_or(rest.len());
        let (run, tail) = rest.split_at(ascii);
        let bytes = run.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match (
                bytes[i],
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (b'%', Some(high), Some(low)) => {
                    decoded.push(high << 4 | low);
                    i += 3;
                }
                (b, _, _) => {
                    decoded.push(b);
                    i += 1;
                }
            }
        }
        out.push_str(&String::from_utf8_lossy(&decoded));
        let other = tail.find(|c: char| c.is_ascii()).unwrap_or(tail.len());
        out.push_str(&tail[..other]);
        rest = &tail[other..];
    }
    out
}
/// `_percent_decoded`: an inline or reference destination is a URL, so a
/// space or a non-ASCII name is written percent-encoded (`My%20Note.md`); a
/// file literally named with the escape keeps precedence. Wikilinks carry the
/// note name verbatim and never come here (#4178).
fn percent_decoded(target: &str, source_dir: &Path) -> String {
    let decoded = unquote(target);
    if decoded == target {
        return decoded;
    }
    let parts = path_syntax::posix_parts(target);
    let literal = if path_syntax::suffix(path_syntax::posix_name(&parts)).is_empty() {
        format!("{target}.md")
    } else {
        target.to_owned()
    };
    if source_dir.join(literal).is_file() {
        return target.to_owned();
    }
    decoded
}
fn is_external_link(target: &str) -> bool {
    let low = target.to_lowercase();
    target.contains("://")
        || ["mailto:", "tel:", "//", "data:"]
            .iter()
            .any(|prefix| low.starts_with(prefix))
}
pub fn resolve_markdown_link(
    raw: &str,
    source_dir: &Path,
    wikilink: bool,
) -> crate::Result<Option<PathBuf>> {
    let raw = strip(raw)
        .split('#')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let mut target = strip(raw).to_owned();
    if target.is_empty() || is_external_link(&target) {
        return Ok(None);
    }
    if !wikilink {
        // Decoding can reveal a scheme or a protocol-relative `%2F%2Fhost`.
        target = percent_decoded(&target, source_dir);
        if is_external_link(&target) {
            return Ok(None);
        }
    }
    let parts = path_syntax::posix_parts(&target);
    let mut suffix = path_syntax::suffix(path_syntax::posix_name(&parts)).to_lowercase();
    if suffix.is_empty() {
        target.push_str(".md");
        suffix = ".md".into();
    }
    if !LINKABLE.contains(&suffix.as_str()) {
        if !wikilink {
            return Ok(None);
        }
        // The suffix may be part of the note name (`[[v1.2 release]]`): keep
        // the link only when `<name>.md` exists. A literal target the engine
        // indexes itself (`[[pic.png]]` beside `pic.png.md`) keeps precedence
        // (#3904).
        let literal = normpath(&source_dir.join(&target));
        if literal.is_file() && crate::detect::classify_file(&literal).is_some() {
            return Ok(None);
        }
        return Ok(
            resolve_markdown_link(&format!("{target}.md"), source_dir, true)
                .ok()
                .flatten()
                .filter(|hit| hit.is_file()),
        );
    }
    let relative = !Path::new(&target).is_absolute();
    let candidate = if relative {
        source_dir.join(&target)
    } else {
        PathBuf::from(&target)
    };
    let resolved = normpath(&candidate);
    if wikilink
        && relative
        && !resolved.is_file()
        && let Some(root) = active_scan_root()
        && let Some(hit) = vault_lookup(&target, &root)?
    {
        return Ok(Some(normpath(&hit)));
    }
    Ok(Some(resolved))
}

pub fn code_span_mention(span: &str) -> Option<(Option<String>, Vec<String>)> {
    let text = strip(span);
    if text.is_empty() || text.contains(' ') {
        return None;
    }
    if let Some(c) = PATH_MENTION.captures(text) {
        return Some((
            Some(c[1].into()),
            c[2].split("::").map(str::to_owned).collect(),
        ));
    }
    let c = BARE_MENTION.captures(text)?;
    if text.contains('.') {
        let ext = format!(".{}", &c[1]);
        if markdown_data::CODE_EXTENSIONS.contains(&ext.as_str())
            || markdown_data::DOC_EXTENSIONS.contains(&ext.as_str())
        {
            return None;
        }
        return Some((
            None,
            text.trim_end_matches(['(', ')'])
                .split('.')
                .map(str::to_owned)
                .collect(),
        ));
    }
    Some((None, vec![c[1].into()]))
}

struct Extraction {
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    calls: Vec<Value>,
    ids: HashSet<String>,
    mentions: HashSet<(String, Option<String>, Vec<String>)>,
    links: HashSet<String>,
}
impl Extraction {
    fn add_node(&mut self, id: &str, label: &str, line: usize, kind: &str) {
        if self.ids.insert(id.into()) {
            self.nodes.push(
                json!({"id":id,"label":label,"file_type":"document","node_kind":kind,
                "source_file":self.path,"source_location":format!("L{line}")}),
            );
        }
    }
    fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        target_file: Option<String>,
    ) {
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED",
            "source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(file) = target_file {
            edge["target_file"] = json!(file);
        }
        self.edges.push(edge);
    }
    fn add_link(
        &mut self,
        file_id: &str,
        dir: &Path,
        raw: &str,
        line: usize,
        wiki: bool,
    ) -> crate::Result<()> {
        if let Some(resolved) = resolve_markdown_link(raw, dir, wiki)? {
            let file = fsutil::path_string(&resolved);
            let target = make_id([file.as_str()]);
            if target != file_id && self.links.insert(target.clone()) {
                let target_file = resolved.is_file().then_some(file);
                self.add_edge(file_id, &target, "references", line, target_file);
            }
        }
        Ok(())
    }
    /// Does `page` name this very file? An empty page name always does.
    fn is_same_page(file_id: &str, dir: &Path, page: &str) -> crate::Result<bool> {
        if strip(page).is_empty() {
            return Ok(true);
        }
        Ok(resolve_markdown_link(page, dir, true)?
            .is_some_and(|resolved| make_id([fsutil::path_string(&resolved).as_str()]) == file_id))
    }
    fn add_mentions(&mut self, owner: &str, text: &str, line: usize) -> crate::Result<()> {
        // A code span needs a backtick; without one the backtracking pattern
        // cannot match (or fail), so it is not run.
        if !text.contains('`') {
            return Ok(());
        }
        for found in CODE.captures_iter(text) {
            let c = found.map_err(|e| format!("Markdown code-span pattern: {e}"))?;
            let span = &c[2];
            if let Some((path, names)) = code_span_mention(span)
                && self
                    .mentions
                    .insert((owner.into(), path.clone(), names.clone()))
            {
                let mut call = json!({"caller_nid":owner,"callee":names.last(),"is_member_call":false,
                    "language":"markdown","context":strip(span),"source_file":self.path,"source_location":format!("L{line}")});
                if let Some(path) = path {
                    call["path"] = json!(path);
                }
                if names.len() > 1 {
                    call["qualifiers"] = json!(&names[..names.len() - 1]);
                }
                self.calls.push(call);
            }
        }
        Ok(())
    }
}

pub fn extract_markdown(path: &Path) -> crate::Result<Value> {
    let source = match super::base::read_source_text(path, true) {
        Ok(source) => source,
        Err(error) => {
            return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)}));
        }
    };
    let str_path = fsutil::path_string(path);
    let file_id = make_id([str_path.as_str()]);
    let stem = file_stem(path);
    let mut out = Extraction {
        path: str_path.clone(),
        nodes: vec![],
        edges: vec![],
        calls: vec![],
        ids: HashSet::new(),
        mentions: HashSet::new(),
        links: HashSet::new(),
    };
    let lines: Vec<&str> = fsutil::split_lines(&source).collect();
    let (fm, body_start) = split_frontmatter(&lines);
    let frontmatter = security::sanitize_metadata(Some(&Value::Object(parse_frontmatter(fm))));
    let parts = path_syntax::posix_parts(&str_path);
    out.add_node(&file_id, path_syntax::posix_name(&parts), 1, "page");
    if !frontmatter.is_empty() {
        out.nodes[0]["frontmatter"] = Value::Object(frontmatter);
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut headings: Vec<(usize, String)> = Vec::new();
    // Same-page heading anchors are resolved after the walk: the heading a
    // `[[#Setup]]` names may be declared further down (upstream PR #3450).
    let mut pending_anchors: Vec<(String, usize, String)> = Vec::new();
    let mut in_code = false;
    for (offset, text) in lines.into_iter().enumerate() {
        let line = offset + 1;
        if strip(text).starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        // Each backtracking pattern needs a literal the line may not have: an
        // inline link a `](`, a wikilink a `[[`. Without it no match (and no
        // matching error) is possible, so the pattern is not run.
        for c in INLINE.captures_iter(if text.contains("](") { text } else { "" }) {
            let c = c.map_err(|e| format!("Markdown inline-link pattern: {e}"))?;
            let target = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
            out.add_link(&file_id, dir, target, line, false)?;
        }
        for c in WIKI.captures_iter(if text.contains("[[") { text } else { "" }) {
            let c = c.map_err(|e| format!("Markdown wikilink pattern: {e}"))?;
            let page = c.get(1).map_or("", |m| m.as_str());
            let anchor = c.get(2).map_or("", |m| strip(m.as_str()));
            if strip(page).is_empty() && anchor.is_empty() {
                continue;
            }
            if !anchor.is_empty() && Extraction::is_same_page(&file_id, dir, page)? {
                let section = headings.last().map_or(&file_id, |(_, id)| id);
                pending_anchors.push((anchor.to_owned(), line, section.clone()));
                continue;
            }
            out.add_link(&file_id, dir, page, line, true)?;
        }
        if text.contains("]:")
            && let Some(c) = REF
                .captures(text)
                .map_err(|e| format!("Markdown reference-link pattern: {e}"))?
        {
            let target = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
            out.add_link(&file_id, dir, target, line, false)?;
        }
        if offset < body_start {
            continue;
        }
        if let Some(c) = HEADING.captures(text) {
            let level = c[1].len();
            let title = strip(&c[2]);
            let mut id = make_id([stem.as_str(), title]);
            if out.ids.contains(&id) {
                id = make_id([stem.as_str(), title, &line.to_string()]);
            }
            out.add_node(&id, title, line, "heading");
            while headings.last().is_some_and(|(depth, _)| *depth >= level) {
                headings.pop();
            }
            let parent = headings
                .last()
                .map(|(_, id)| id.as_str())
                .unwrap_or(&file_id);
            out.add_edge(parent, &id, "contains", line, None);
            out.add_mentions(&id, title, line)?;
            headings.push((level, id));
        } else {
            let owner = headings
                .last()
                .map(|(_, id)| id.as_str())
                .unwrap_or(&file_id);
            out.add_mentions(owner, text, line)?;
        }
    }
    // Existence-gated like document links: an anchor naming no heading stays edge-free.
    let mut seen_anchor_pairs: HashSet<(String, String)> = HashSet::new();
    for (anchor, line, source) in pending_anchors {
        let heading = make_id([stem.as_str(), anchor.as_str()]);
        if !out.ids.contains(&heading) || heading == source {
            continue;
        }
        if !seen_anchor_pairs.insert((source.clone(), heading.clone())) {
            continue;
        }
        out.add_edge(&source, &heading, "references", line, None);
    }
    Ok(
        json!({"nodes":out.nodes,"edges":out.edges,"raw_calls":out.calls,"input_tokens":0,"output_tokens":0}),
    )
}

#[cfg(test)]
mod tests {
    use super::extract_markdown;

    #[test]
    fn wikilink_index_prunes_ignored_trees_and_applies_nested_file_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".graphicleignore"), "bigdata/\nprivate.md\n").unwrap();
        std::fs::create_dir_all(root.join("bigdata/.graphicleignore")).unwrap();
        // If the ignored tree is entered, its invalid ignore-file path fails
        // indexing. Pruning must happen before loading the child's rules.
        std::fs::write(root.join("bigdata/secret.md"), "secret").unwrap();
        std::fs::create_dir(root.join("notes")).unwrap();
        std::fs::write(root.join("notes/.gitignore"), "hidden.md\n").unwrap();
        for name in ["target.md", "private.md", "hidden.md"] {
            std::fs::write(root.join("notes").join(name), "# Fixture").unwrap();
        }
        let index = super::build_link_index(root).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.contains_key("target.md"));
    }

    fn triples(result: &serde_json::Value, relation: &str) -> Vec<(String, String)> {
        result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == relation)
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                )
            })
            .collect()
    }

    #[test]
    fn same_page_heading_anchors_link_sections_and_page_anchors_keep_the_page() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.md"), "# Other\n\n## Sec\n").unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(
            &notes,
            "# Intro\nsee [[#Setup]] and [[#Setup]] again, [[Other#Sec]], [[#Missing]], [[]] and [[|alias]]\n\n## Setup\n",
        )
        .unwrap();
        let result = extract_markdown(&notes).unwrap();
        let refs = triples(&result, "references");
        let other =
            super::make_id([super::fsutil::path_string(&dir.path().join("other.md")).as_str()]);
        let notes_id = super::make_id([super::fsutil::path_string(&notes).as_str()]);
        let heading = |label: &str| -> String {
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["label"] == label)
                .map(|n| n["id"].as_str().unwrap().into())
                .unwrap()
        };
        assert_eq!(
            refs,
            [(notes_id, other), (heading("Intro"), heading("Setup"))]
        );
    }

    #[test]
    fn table_escaped_alias_pipe_is_not_part_of_the_wikilink_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes/target.md"), "# T\n").unwrap();
        let plain = dir.path().join("plain.md");
        let escaped = dir.path().join("escaped.md");
        std::fs::write(&plain, "| [[notes/Target|Alias]] |\n").unwrap();
        std::fs::write(&escaped, "| [[notes/Target\\|Alias]] |\n").unwrap();
        let expected =
            super::make_id([
                super::fsutil::path_string(&dir.path().join("notes/Target.md")).as_str(),
            ]);
        for path in [&plain, &escaped] {
            let refs = triples(&extract_markdown(path).unwrap(), "references");
            assert_eq!(refs.len(), 1, "{path:?}: {refs:?}");
            assert_eq!(refs[0].1, expected, "{path:?}");
        }
    }
}
