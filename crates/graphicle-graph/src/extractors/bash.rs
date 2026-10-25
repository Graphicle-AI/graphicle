// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/bash.py
//! Complete Graphify Bash extraction, including source-path inference and call provenance.

#[derive(Clone, Copy)]
pub struct EdgeEvidence<'a> {
    pub line: usize,
    pub inferred: bool,
    pub context: Option<&'a str>,
    pub target_file: Option<&'a str>,
}
use super::base::{file_stem, make_id, read_text};
use crate::{Result, fsutil, io_compat, path_syntax, paths, regex_compat::compile, security};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};
use tree_sitter::Node;
static LEADING_EXPANSION: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^(?:(?:\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*)/?)+"));
static SOURCE_DIRNAME: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"\$\(dirname\s+"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?"\)"#));
static LEADING_VAR: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^\$\{([A-Za-z_][A-Za-z0-9_]*)[^}]*\}|^\$([A-Za-z_][A-Za-z0-9_]*)"));
static DIRNAME_IDIOM: LazyLock<Regex> = LazyLock::new(|| compile(r"dirname[^)]*\)((?:/\.\.)*)"));
const SOURCE_COMMANDS: &[&str] = &["source", "."];
const RUNNERS: &[&str] = &["bash", "sh", "zsh", "ksh", "dash"];
/// Non-shell interpreters a pipeline orchestrator hands a script path to
/// (Graphify 0d2ade6, #3802): `python3 scripts/stage_one.py`, `node build.js`.
const SCRIPT_INVOKERS: &[&str] = &[
    "python", "python3", "python2", "node", "nodejs", "ruby", "perl",
];
const INVOKABLE_SCRIPT_EXTENSIONS: &[&str] = &[".py", ".js", ".mjs", ".cjs", ".rb", ".pl", ".sh"];
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
fn parent(path: &Path) -> PathBuf {
    let mut parts = path_syntax::posix_parts(&fsutil::path_string(path));
    if !path_syntax::posix_name(&parts).is_empty() {
        parts.pop();
    }
    PathBuf::from(path_syntax::posix_join(&parts))
}
fn resolve(path: &Path) -> Result<PathBuf> {
    fsutil::resolve_missing(path).map_err(crate::Error::from)
}
pub fn bash_source_suffix(raw: &str, allow_dotdot: bool) -> Option<String> {
    let stripped = LEADING_EXPANSION.replacen(raw, 1, "");
    let suffix = stripped.trim_start_matches('/');
    if suffix.is_empty()
        || suffix.contains('$')
        || (!allow_dotdot && suffix.split('/').any(|s| s == ".."))
    {
        None
    } else {
        Some(suffix.into())
    }
}
pub fn within_tree(ceiling: &Path, target: &Path) -> bool {
    let c = paths::normpath(&fsutil::path_string(ceiling));
    let t = paths::normpath(&fsutil::path_string(target));
    t == c || t.starts_with(&format!("{c}{}", std::path::MAIN_SEPARATOR))
}
pub fn bash_assignment_base(value: &str, script_dir: &Path) -> Option<PathBuf> {
    let value = trim(value).trim_matches(['\'', '"']);
    if value.is_empty() {
        return None;
    }
    if value.contains("dirname") && (value.contains("BASH_SOURCE") || value.contains("$0")) {
        let count = DIRNAME_IDIOM
            .captures(value)
            .map_or(0, |c| c[1].matches("..").count());
        let mut base = script_dir.to_path_buf();
        for _ in 0..count {
            base = parent(&base);
        }
        return Some(base);
    }
    if value.contains(['$', '`']) {
        return None;
    }
    let candidate = PathBuf::from(value);
    Some(if candidate.is_absolute() {
        candidate
    } else {
        script_dir.join(candidate)
    })
}
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn inside_expansion(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if matches!(n.kind(), "command_substitution" | "process_substitution") {
            return true;
        }
        current = n.parent();
    }
    false
}
fn call_allowed(node: Node<'_>) -> bool {
    let mut current = node.parent();
    let mut substitution = false;
    while let Some(n) = current {
        match n.kind() {
            "process_substitution" => return false,
            "command_substitution" => substitution = true,
            "variable_assignment" if substitution => return true,
            _ => (),
        }
        current = n.parent();
    }
    !substitution
}
struct Extractor<'tree, 'source> {
    source: &'source [u8],
    path: PathBuf,
    path_text: String,
    stem: String,
    file: String,
    entry: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    bash_sources: Vec<Value>,
    raw_seen: HashSet<(String, String)>,
    seen: HashSet<String>,
    bodies: Vec<(String, Option<Node<'tree>>)>,
    defined: HashSet<String>,
    bases: HashMap<String, PathBuf>,
}
impl<'tree> Extractor<'tree, '_> {
    fn node(&mut self, id: &str, label: &str, line: usize, kind: &str) {
        if !id.is_empty() && self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path_text,"source_location":format!("L{line}"),"metadata":security::sanitize_metadata(Some(&json!({"language":"bash","kind":kind})))}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, context: EdgeEvidence<'_>) {
        let EdgeEvidence {
            line,
            inferred,
            context,
            target_file,
        } = context;

        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":if inferred{"INFERRED"}else{"EXTRACTED"},"source_file":self.path_text,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context {
            edge["context"] = c.into();
        }
        if let Some(file) = target_file {
            edge["target_file"] = file.into();
        }
        self.edges.push(edge);
    }
    fn literal(&self, node: Node<'tree>) -> Option<String> {
        let text = read_text(node, self.source);
        let mut raw = trim(&text);
        if raw.is_empty() {
            return None;
        }
        if (raw.starts_with('\'') && raw.ends_with('\''))
            || (raw.starts_with('"') && raw.ends_with('"'))
        {
            raw = if raw.len() == 1 {
                ""
            } else {
                &raw[1..raw.len() - 1]
            };
        }
        if ["$", "`", "$(", "<(", ">", "|", ";", "&"]
            .iter()
            .any(|token| raw.contains(token))
        {
            None
        } else {
            Some(raw.into())
        }
    }
    fn function_name(&self, node: Node<'tree>) -> Option<String> {
        children(node)
            .find(|n| n.kind() == "word")
            .and_then(|n| self.literal(n))
    }
    fn prescan(&mut self, node: Node<'tree>) {
        if node.kind() == "function_definition"
            && let Some(name) = self.function_name(node).filter(|s| !s.is_empty())
        {
            self.defined.insert(name);
        }
        for child in children(node) {
            self.prescan(child);
        }
    }
    fn source_record(&mut self, target: &str, line: usize) {
        self.bash_sources.push(json!({"target_path":target,"source_file":self.path_text,"source_location":format!("L{line}")}));
    }
    fn import_resolved(&mut self, resolved: &Path, record: &str, line: usize, inferred: bool) {
        let target = fsutil::path_string(resolved);
        let id = make_id([target.as_str()]);
        self.edge(
            &self.file.clone(),
            &id,
            "imports_from",
            EdgeEvidence {
                line,
                inferred,
                context: Some("import"),
                target_file: Some(&target),
            },
        );
        self.source_record(record, line);
    }
    fn source_command(&mut self, raw: &str, line: usize) -> Result<()> {
        let script_dir = parent(&self.path);
        if raw.starts_with(['.', '/']) {
            let resolved = resolve(&script_dir.join(raw))?;
            if resolved.exists() {
                self.import_resolved(&resolved, raw, line, false);
            }
        } else if raw.contains('$') {
            let dirname = SOURCE_DIRNAME
                .captures(raw)
                .filter(|c| c.get(0).unwrap().start() == 0);
            if let Some(captured) = dirname {
                let variable = &captured[1];
                let base = if let Some(base) = self.bases.get(variable) {
                    parent(base)
                } else {
                    parent(&script_dir)
                };
                let suffix = raw[captured.get(0).unwrap().end()..].trim_start_matches('/');
                if !suffix.is_empty()
                    && !suffix.contains('$')
                    && !suffix.split('/').any(|s| s == "..")
                {
                    let resolved = resolve(&base.join(suffix))?;
                    if resolved.is_file() {
                        self.import_resolved(
                            &resolved,
                            &fsutil::path_string(&resolved),
                            line,
                            true,
                        );
                    }
                }
            } else {
                let variable = LEADING_VAR
                    .captures(raw)
                    .and_then(|c| c.get(1).or_else(|| c.get(2)))
                    .map(|m| m.as_str().to_string());
                let tracked = variable
                    .as_ref()
                    .and_then(|name| self.bases.get(name))
                    .cloned();
                if let Some(suffix) = bash_source_suffix(raw, tracked.is_some()) {
                    let base = tracked.as_ref().unwrap_or(&script_dir);
                    let (resolved, ceiling) = if tracked.is_some() {
                        (
                            PathBuf::from(paths::normpath(&fsutil::path_string(
                                &base.join(&suffix),
                            ))),
                            parent(base),
                        )
                    } else {
                        (resolve(&base.join(&suffix))?, base.clone())
                    };
                    if within_tree(&ceiling, &resolved) && resolved.is_file() {
                        self.import_resolved(
                            &resolved,
                            &fsutil::path_string(&resolved),
                            line,
                            true,
                        );
                    }
                }
            }
        } else {
            let sibling = if !raw.is_empty() && script_dir.join(raw).is_file() {
                resolve(&script_dir.join(raw)).ok()
            } else {
                None
            };
            if let Some(sibling) = sibling {
                self.import_resolved(&sibling, raw, line, true);
            } else {
                let id = make_id([raw]);
                if !id.is_empty() {
                    self.edge(
                        &self.file.clone(),
                        &id,
                        "imports",
                        EdgeEvidence {
                            line,
                            inferred: false,
                            context: Some("import"),
                            target_file: None,
                        },
                    );
                }
            }
        }
        Ok(())
    }
    fn command(&mut self, node: Node<'tree>, parent_id: &str) -> Result<()> {
        if inside_expansion(node) {
            return Ok(());
        }
        let Some(name_node) = node.child_by_field_name("name").or_else(|| node.child(0)) else {
            return Ok(());
        };
        let command = self.literal(name_node);
        let args = children(node)
            .filter(|n| matches!(n.kind(), "word" | "string" | "concatenation") && *n != name_node)
            .collect::<Vec<_>>();
        let line = node.start_position().row + 1;
        let is_defined = command.as_ref().is_some_and(|s| self.defined.contains(s));
        if command
            .as_deref()
            .is_some_and(|s| SOURCE_COMMANDS.contains(&s))
            && !is_defined
        {
            if let Some(arg) = args.first() {
                let raw = read_text(*arg, self.source);
                self.source_command(trim(&raw).trim_matches(['\'', '"']), line)?;
            }
        } else if !is_defined {
            let mut raw = command
                .as_ref()
                .filter(|s| !s.is_empty() && s.ends_with(".sh"))
                .cloned();
            if command.as_deref().is_some_and(|s| RUNNERS.contains(&s))
                && let Some(arg) = args.first()
            {
                raw = self.literal(*arg);
            }
            if raw.is_none() {
                let candidate = read_text(name_node, self.source);
                let candidate = trim(&candidate).trim_matches(['\'', '"']);
                if candidate.ends_with(".sh") && candidate.contains('$') {
                    raw = bash_source_suffix(candidate, false).filter(|s| !s.is_empty());
                }
            }
            if let Some(raw) = raw.filter(|s| !s.is_empty() && s.ends_with(".sh")) {
                let resolved = resolve(&parent(&self.path).join(raw))?;
                if resolved.is_file() {
                    let mut target = resolved.clone();
                    if !self.path.is_absolute() {
                        let cwd = resolve(&std::env::current_dir().map_err(crate::Error::from)?)?;
                        if let Ok(relative) = resolved.strip_prefix(cwd) {
                            target = relative.to_path_buf();
                        }
                    }
                    let target = fsutil::path_string(&target);
                    let id = make_id([target.as_str()]) + "__entry";
                    let caller = if parent_id == self.file {
                        self.entry.clone()
                    } else {
                        parent_id.into()
                    };
                    self.edge(
                        &caller,
                        &id,
                        "calls",
                        EdgeEvidence {
                            line,
                            inferred: false,
                            context: Some("script_invocation"),
                            target_file: Some(&target),
                        },
                    );
                }
            }
        }
        // A script run through a non-shell interpreter, or through a bare
        // variable standing in for one (`"$PYTHON" stage.py`), is a real
        // dependency on that file; independent of the `.sh` handling above.
        let known_interpreter = command
            .as_deref()
            .is_some_and(|s| SCRIPT_INVOKERS.contains(&s))
            && !is_defined;
        let word = read_text(name_node, self.source);
        let mut word = trim(&word);
        if let Some(quote) = word.chars().next().filter(|c| matches!(c, '\'' | '"'))
            && word.ends_with(quote)
        {
            word = word.get(1..word.len().saturating_sub(1)).unwrap_or("");
        }
        // A bare interpreter variable has no path separator: `"$DIR/run.sh"
        // x.py` runs run.sh with x.py as its argument (Graphify 0110d96).
        let interpreter_variable = command.is_none()
            && word.starts_with('$')
            && !word.contains("$(")
            && !word.contains('/');
        if (known_interpreter || interpreter_variable)
            && let Some(arg) = args.first()
            && let Some(raw) = self
                .literal(*arg)
                .filter(|raw| INVOKABLE_SCRIPT_EXTENSIONS.iter().any(|e| raw.ends_with(e)))
        {
            let resolved = resolve(&parent(&self.path).join(&raw))?;
            if resolved.is_file() {
                let mut target = resolved.clone();
                if !self.path.is_absolute() {
                    let cwd = resolve(&std::env::current_dir().map_err(crate::Error::from)?)?;
                    if let Ok(relative) = resolved.strip_prefix(cwd) {
                        target = relative.to_path_buf();
                    }
                }
                let target = fsutil::path_string(&target);
                let mut id = make_id([target.as_str()]);
                if resolved.extension().is_some_and(|e| e == "sh") {
                    id += "__entry";
                }
                let caller = if parent_id == self.file {
                    self.entry.clone()
                } else {
                    parent_id.into()
                };
                self.edge(
                    &caller,
                    &id,
                    "invokes",
                    EdgeEvidence {
                        line,
                        inferred: true,
                        context: Some("script_invocation"),
                        target_file: Some(&target),
                    },
                );
            }
        }
        Ok(())
    }
    fn walk(&mut self, node: Node<'tree>, parent_id: &str) -> Result<()> {
        if node.kind() == "function_definition" {
            if let Some(name) = self.function_name(node).filter(|s| !s.is_empty()) {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                let line = node.start_position().row + 1;
                self.node(&id, &format!("{name}()"), line, "bash_function");
                self.edge(
                    parent_id,
                    &id,
                    "defines",
                    EdgeEvidence {
                        line,
                        inferred: false,
                        context: None,
                        target_file: None,
                    },
                );
                self.defined.insert(name);
                let body = children(node).find(|n| n.kind() == "compound_statement");
                self.bodies.push((id.clone(), body));
                if let Some(body) = body {
                    self.walk(body, &id)?;
                }
            }
            return Ok(());
        }
        if node.kind() == "command" {
            return self.command(node, parent_id);
        }
        if node.kind() == "declaration_command" {
            if node.parent().is_some_and(|n| n.kind() == "program") {
                for child in children(node).filter(|n| n.kind() == "variable_assignment") {
                    if let Some(name) = child.child_by_field_name("name") {
                        let text = read_text(name, self.source);
                        let name = trim(&text);
                        if !name.is_empty() {
                            let id = make_id([self.stem.as_str(), name]);
                            let line = child.start_position().row + 1;
                            self.node(&id, name, line, "code");
                            self.edge(
                                &self.file.clone(),
                                &id,
                                "defines",
                                EdgeEvidence {
                                    line,
                                    inferred: false,
                                    context: None,
                                    target_file: None,
                                },
                            );
                        }
                    }
                }
            }
            return Ok(());
        }
        for child in children(node) {
            self.walk(child, parent_id)?;
        }
        Ok(())
    }
    fn walk_calls(
        &mut self,
        node: Node<'tree>,
        caller: &str,
        seen: &mut HashSet<(String, String)>,
    ) {
        for child in children(node) {
            if child.kind() == "function_definition" {
                continue;
            }
            if child.kind() == "command"
                && call_allowed(child)
                && let Some(name_node) =
                    child.child_by_field_name("name").or_else(|| child.child(0))
                && let Some(name) = self.literal(name_node).filter(|s| !s.is_empty())
            {
                if self.defined.contains(&name) {
                    let target = make_id([self.stem.as_str(), name.as_str()]);
                    if !target.is_empty() && seen.insert((caller.into(), target.clone())) {
                        self.edge(
                            caller,
                            &target,
                            "calls",
                            EdgeEvidence {
                                line: child.start_position().row + 1,
                                inferred: false,
                                context: Some("call"),
                                target_file: None,
                            },
                        );
                    }
                } else if !SOURCE_COMMANDS.contains(&name.as_str())
                    && !RUNNERS.contains(&name.as_str())
                    && self.raw_seen.insert((caller.into(), name.clone()))
                {
                    self.raw_calls.push(json!({"language":"bash","callee":name,"caller_nid":caller,"source_file":self.path_text,"source_location":format!("L{}",child.start_position().row+1)}));
                }
            }
            self.walk_calls(child, caller, seen);
        }
    }
}
pub fn extract_bash(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(error) => {
            return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)}));
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&tree_sitter_bash::LANGUAGE.into()) {
        return Ok(json!({"nodes":[],"edges":[],"error":error.to_string()}));
    }
    let Some(tree) = parser.parse(&source, None) else {
        return Ok(json!({"nodes":[],"edges":[],"error":"Bash parse cancelled"}));
    };
    let text = fsutil::path_string(path);
    let parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts);
    let file = make_id([text.as_str()]);
    let entry = file.clone() + "__entry";
    let mut ex = Extractor {
        source: &source,
        path: path.into(),
        path_text: text,
        stem: file_stem(path),
        file: file.clone(),
        entry: entry.clone(),
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        bash_sources: vec![],
        raw_seen: HashSet::new(),
        seen: HashSet::new(),
        bodies: vec![],
        defined: HashSet::new(),
        bases: HashMap::new(),
    };
    ex.node(&file, name, 1, "file");
    ex.node(&entry, &format!("{name} script"), 1, "bash_entrypoint");
    ex.edge(
        &file,
        &entry,
        "contains",
        EdgeEvidence {
            line: 1,
            inferred: false,
            context: None,
            target_file: None,
        },
    );
    ex.prescan(tree.root_node());
    for assignment in children(tree.root_node()).filter(|n| n.kind() == "variable_assignment") {
        if let (Some(name), Some(value)) = (
            assignment.child_by_field_name("name"),
            assignment.child_by_field_name("value"),
        ) {
            let text = read_text(name, &source);
            let name = trim(&text);
            if !name.is_empty()
                && let Some(base) = bash_assignment_base(&read_text(value, &source), &parent(path))
            {
                ex.bases.insert(name.into(), base);
            }
        }
    }
    ex.walk(tree.root_node(), &file)?;
    ex.walk_calls(tree.root_node(), &entry, &mut HashSet::new());
    for (caller, body) in ex.bodies.clone() {
        if let Some(body) = body {
            ex.walk_calls(body, &caller, &mut HashSet::new());
        }
    }
    Ok(
        json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"bash_sources":ex.bash_sources}),
    )
}
#[cfg(test)]
mod tests {
    #[test]
    fn grammar_matches_pinned_reference() {
        let l: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 280);
        assert_eq!(l.parse_state_count(), 7571);
        assert_eq!(l.abi_version(), 15);
    }
}
