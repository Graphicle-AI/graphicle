//! reStructuredText documents (opt-in: `--docs` / `GRAPHICLE_DOCS=1`).
//!
//! Graphify extracts Markdown pages deterministically (headings, links and
//! code-span mentions resolved to code, `markdown_resolution`) but leaves
//! reStructuredText, the format of Sphinx documentation, to the LLM pass. This
//! extractor gives `.rst` pages the same structure as Markdown pages, so the
//! same resolver links them to the code they document:
//!
//! - a `page` node per file and a `heading` node per section title (an
//!   underlined, optionally overlined line), nested by the order in which
//!   title styles first appear, joined by `contains` edges;
//! - mentions from Sphinx roles (`:func:`, `:class:`, `:meth:`, `:attr:`,
//!   `:exc:`, `:data:`, `:obj:`, `:const:`, with or without `py:`), from
//!   object directives (`.. autofunction:: pkg.f`, `.. class:: C`) and from
//!   inline literals shaped like code (``` ``pkg.f`` ```, ``` ``f()`` ```,
//!   ``` ``QuerySet`` ```), owned by the enclosing section;
//! - `references` edges to files named by `.. literalinclude::` and
//!   `.. include::` when they exist.
//!
//! Literal blocks (after `::`, `.. code-block::` and similar) are skipped.
//! Dotted targets keep every part: the resolver accepts a candidate only
//! when each qualifier names one of its enclosing definitions or a part of
//! its path, and only mentions that resolve to exactly one code definition
//! become edges. Django's `.txt` Sphinx sources count as pages when a
//! `conf.py` above them declares them (`source_suffix`).
use super::base::file_stem;
use crate::{fsutil, ids::make_id, io_compat, path_syntax, regex_compat};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// `--docs` / `GRAPHICLE_DOCS=1`: extract `.rst` pages.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) || std::env::var("GRAPHICLE_DOCS").is_ok_and(|v| v == "1")
}

static SPHINX_TXT: LazyLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, bool>>> =
    LazyLock::new(Default::default);

/// A `.txt` file is a reStructuredText page when a Sphinx `conf.py` in one of
/// its directories declares `.txt` sources (Django's documentation does).
pub fn is_sphinx_txt(path: &Path) -> bool {
    let mut dir = path.parent();
    for _ in 0..8 {
        let Some(d) = dir else { break };
        let mut cache = SPHINX_TXT.lock().unwrap_or_else(|e| e.into_inner());
        let hit = *cache.entry(d.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(d.join("conf.py")).is_ok_and(|conf| {
                conf.lines()
                    .any(|l| l.trim_start().starts_with("source_suffix") && l.contains(".txt"))
            })
        });
        if hit {
            return true;
        }
        dir = d.parent();
    }
    false
}

static ROLE: LazyLock<Regex> = LazyLock::new(|| {
    regex_compat::compile(
        r":(?:py:)?(?:func|meth|class|attr|exc|data|obj|const|decorator|property):`([^`]+)`",
    )
});
static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    regex_compat::compile(
        r"^\s*\.\.\s+(?:py:)?(?:auto)?(?:function|class|method|attribute|exception|data|decorator|property|classmethod|staticmethod)::\s*([^\s(]+)",
    )
});
static INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| regex_compat::compile(r"^\s*\.\.\s+(?:literalinclude|include)::\s*(\S+)"));
static LITERAL: LazyLock<Regex> = LazyLock::new(|| regex_compat::compile(r"``([^`]+)``"));
static CODE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    regex_compat::compile(
        r"^\s*\.\.\s+(?:code-block|code|sourcecode|doctest|testcode|testoutput|ipython|highlight-block|parsed-literal|math)::",
    )
});
const UNDERLINE: &str = "=-`:'\"~^_*+#<>.";

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A section underline (or overline) for a title of `width` characters.
fn is_rule(line: &str, width: usize) -> Option<char> {
    let t = line.trim_end();
    let c = t.chars().next()?;
    (UNDERLINE.contains(c) && t.chars().all(|x| x == c) && t.chars().count() >= width.max(2))
        .then_some(c)
}

/// The names a role or directive target is looked up by: its dotted parts.
/// The resolver keeps a candidate only when every qualifier is an enclosing
/// definition's name or a part of the candidate's path, so
/// `django.test.signals.setting_changed` cannot land on another
/// `setting_changed` and `time.strftime` (not in the repository) on none.
fn target_names(target: &str) -> Option<Vec<String>> {
    let t = target.trim();
    // `title <target>`
    let t = match (t.rfind('<'), t.ends_with('>')) {
        (Some(at), true) => &t[at + 1..t.len() - 1],
        _ => t,
    };
    let t = t.trim_start_matches(['~', '!', '.']).trim_end_matches("()");
    let parts = t.split('.').filter(|p| !p.is_empty()).collect::<Vec<_>>();
    let ident = |p: &str| {
        p.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && p.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    if parts.is_empty() || !parts.iter().all(|p| ident(p)) {
        return None;
    }
    Some(parts.into_iter().map(str::to_string).collect())
}

/// Whether an inline literal is looked up at all. Sphinx pages name code with
/// roles; their literals are mostly parameters, settings, headers, commands
/// and example models (``` ``autoescape`` ```, ``` ``current_app`` ```,
/// ``` ``Host`` ```, ``` ``Blog`` ```), which the resolver would tie to
/// whichever single definition shares the name. On Django's documentation
/// such bare words linked correctly about half the time. So a literal counts
/// only when it is shaped like a reference to code: a dotted path, a call
/// (``` ``chars()`` ```) or a CamelCase name with two or more capitals
/// (``` ``QuerySet`` ```).
fn code_shaped(literal: &str) -> bool {
    let text = literal.trim();
    let name = text.strip_suffix("()").unwrap_or(text);
    name.len() < text.len()
        || name.contains('.')
        || name.contains("::")
        || (name.starts_with(char::is_uppercase)
            && name.chars().filter(|c| c.is_uppercase()).count() >= 2
            && name.chars().any(char::is_lowercase))
}

/// Whether dotted `names` spell a module or package of the project the page
/// belongs to (``` ``django.contrib.auth`` ```): a module is not a
/// definition, and its last part must not land on a function of that name.
fn names_module(dir: &Path, names: &[String]) -> bool {
    if names.len() < 2 {
        return false;
    }
    let relative = names.join("/");
    dir.ancestors().take(8).any(|d| {
        let path = d.join(&relative);
        path.is_dir() || path.with_extension("py").is_file()
    })
}

struct Page {
    path: String,
    dir: std::path::PathBuf,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    calls: Vec<Value>,
    ids: HashSet<String>,
    seen: HashSet<(String, Vec<String>)>,
    links: HashSet<String>,
}

impl Page {
    fn node(&mut self, id: &str, label: &str, line: usize, kind: &str) {
        if self.ids.insert(id.into()) {
            self.nodes.push(json!({"id": id, "label": label, "file_type": "document",
                "node_kind": kind, "source_file": self.path, "source_location": format!("L{line}")}));
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        file: Option<String>,
    ) {
        let mut edge = json!({"source": source, "target": target, "relation": relation,
            "confidence": "EXTRACTED", "source_file": self.path,
            "source_location": format!("L{line}"), "weight": 1.0});
        if let Some(file) = file {
            edge["target_file"] = json!(file);
        }
        self.edges.push(edge);
    }
    fn mention(
        &mut self,
        owner: &str,
        names: Vec<String>,
        path: Option<String>,
        context: &str,
        line: usize,
    ) {
        if names.is_empty() || !self.seen.insert((owner.into(), names.clone())) {
            return;
        }
        let mut call = json!({"caller_nid": owner, "callee": names.last(), "is_member_call": false,
            "language": "markdown", "context": context, "source_file": self.path,
            "source_location": format!("L{line}")});
        if let Some(path) = path {
            call["path"] = json!(path);
        }
        if names.len() > 1 {
            call["qualifiers"] = json!(&names[..names.len() - 1]);
        }
        self.calls.push(call);
    }
    fn mentions(&mut self, owner: &str, text: &str, line: usize) {
        for c in ROLE.captures_iter(text) {
            if let Some(names) = target_names(&c[1]) {
                self.mention(owner, names, None, &c[0], line);
            }
        }
        let without_roles = ROLE.replace_all(text, "");
        for c in LITERAL.captures_iter(&without_roles) {
            if !code_shaped(&c[1]) {
                continue;
            }
            if let Some((path, names)) = super::markdown::code_span_mention(&c[1]) {
                if path.is_none() && !c[1].trim().ends_with("()") && names_module(&self.dir, &names)
                {
                    continue;
                }
                self.mention(owner, names, path, &c[1], line);
            }
        }
    }
}

/// Nodes, edges and mention records of one `.rst` page (see module docs).
pub fn extract_rst(path: &Path) -> crate::Result<Value> {
    let source = match fsutil::read_text_lossy(path) {
        Ok(source) => source,
        Err(error) => {
            return Ok(
                json!({"nodes": [], "edges": [], "error": io_compat::read_error(&error, path)}),
            );
        }
    };
    let str_path = fsutil::path_string(path);
    let file_id = make_id([str_path.as_str()]);
    let stem = file_stem(path);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut page = Page {
        path: str_path.clone(),
        dir: dir.to_path_buf(),
        nodes: vec![],
        edges: vec![],
        calls: vec![],
        ids: HashSet::new(),
        seen: HashSet::new(),
        links: HashSet::new(),
    };
    let parts = path_syntax::posix_parts(&str_path);
    page.node(&file_id, path_syntax::posix_name(&parts), 1, "page");
    let lines = fsutil::split_lines(&source).collect::<Vec<_>>();
    // Title styles (underline char, overlined) in order of first use.
    let mut styles: Vec<(char, bool)> = vec![];
    let mut stack: Vec<(usize, String)> = vec![];
    // Indentation of the literal block being skipped, once it starts.
    let mut literal: Option<Option<usize>> = None;
    let mut i = 0;
    while i < lines.len() {
        let text = lines[i];
        let line = i + 1;
        if let Some(block) = literal {
            if text.trim().is_empty() {
                i += 1;
                continue;
            }
            match block {
                None if indent(text) > 0 => literal = Some(Some(indent(text))),
                Some(width) if indent(text) >= width => {}
                None => literal = None,
                Some(_) => literal = None,
            }
            if literal.is_some() {
                i += 1;
                continue;
            }
        }
        // A section title: text over a rule at least as long (and an
        // optional matching overline).
        let title = text.trim();
        if !title.is_empty()
            && indent(text) == 0
            && is_rule(text, 1).is_none()
            && let Some(c) = lines
                .get(i + 1)
                .and_then(|n| is_rule(n, title.chars().count()))
        {
            let over = i > 0 && is_rule(lines[i - 1], title.chars().count()) == Some(c);
            let style = (c, over);
            let level = styles.iter().position(|s| *s == style).unwrap_or_else(|| {
                styles.push(style);
                styles.len() - 1
            });
            let mut id = make_id([stem.as_str(), title]);
            if page.ids.contains(&id) {
                id = make_id([stem.as_str(), title, &line.to_string()]);
            }
            page.node(&id, title, line, "heading");
            while stack.last().is_some_and(|(depth, _)| *depth >= level) {
                stack.pop();
            }
            let parent = stack.last().map_or(file_id.clone(), |(_, id)| id.clone());
            page.edge(&parent, &id, "contains", line, None);
            page.mentions(&id.clone(), title, line);
            stack.push((level, id));
            i += 2;
            continue;
        }
        let owner = stack.last().map_or(file_id.clone(), |(_, id)| id.clone());
        if let Some(c) = INCLUDE.captures(text) {
            let target = crate::detect_ignore::absolute_lexical(&dir.join(c[1].trim()));
            if target.is_file() {
                let file = fsutil::path_string(&target);
                let id = make_id([file.as_str()]);
                if id != file_id && page.links.insert(id.clone()) {
                    page.edge(&file_id, &id, "references", line, Some(file));
                }
            }
        } else if let Some(c) = DIRECTIVE.captures(text) {
            if let Some(names) = target_names(&c[1]) {
                page.mention(&owner, names, None, text.trim(), line);
            }
        } else {
            page.mentions(&owner, text, line);
        }
        if CODE_DIRECTIVE.is_match(text)
            || (text.trim_end().ends_with("::") && !text.trim_start().starts_with(".."))
        {
            literal = Some(None);
        }
        i += 1;
    }
    Ok(
        json!({"nodes": page.nodes, "edges": page.edges, "raw_calls": page.calls,
        "input_tokens": 0, "output_tokens": 0}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_roles_directives_and_literal_blocks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("example.py"), "print(1)\n").unwrap();
        std::fs::create_dir_all(dir.path().join("click")).unwrap();
        std::fs::write(dir.path().join("click/core.py"), "").unwrap();
        let page = dir.path().join("api.rst");
        std::fs::write(
            &page,
            "=========\nAPI guide\n=========\n\nUse :func:`click.echo` and ``Group.command``.\n\n\
             Commands\n--------\n\n.. autoclass:: click.core.Command\n   :members:\n\n\
             See :meth:`~click.core.Command.invoke` and :class:`Context <click.Context>`.\n\n\
             Example::\n\n    parse_args(ignored)\n\nBack to ``make_context()``, not ``ctx``,\n\
             ``Host``, ``DEBUG`` or the ``click.core`` module; ``MultiCommand`` is code.\n\n\
             .. literalinclude:: example.py\n",
        )
        .unwrap();
        let out = extract_rst(&page).unwrap();
        let labels = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["label"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["api.rst", "API guide", "Commands"]);
        let contains = out["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "contains")
            .count();
        assert_eq!(contains, 2);
        let mut mentions = out["raw_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let mut names = c["qualifiers"]
                    .as_array()
                    .map(|q| q.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                    .unwrap_or_else(Vec::new);
                names.push(c["callee"].as_str().unwrap().to_string());
                names.join(".")
            })
            .collect::<Vec<_>>();
        mentions.sort();
        assert_eq!(
            mentions,
            [
                "Group.command",
                "MultiCommand",
                "click.Context",
                "click.core.Command",
                "click.core.Command.invoke",
                "click.echo",
                "make_context"
            ]
        );
        assert!(
            out["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["relation"] == "references"
                    && e["target_file"].as_str().unwrap().ends_with("example.py"))
        );
    }

    #[test]
    fn target_names_are_the_dotted_parts() {
        assert_eq!(
            target_names("pkg.mod.func"),
            Some(vec!["pkg".into(), "mod".into(), "func".into()])
        );
        assert_eq!(
            target_names("~pkg.Cls.meth()"),
            Some(vec!["pkg".into(), "Cls".into(), "meth".into()])
        );
        assert_eq!(
            target_names("Title <pkg.Cls>"),
            Some(vec!["pkg".into(), "Cls".into()])
        );
        assert_eq!(target_names("not a name!"), None);
    }

    #[test]
    fn only_code_shaped_literals_are_mentions() {
        for code in [
            "QuerySet",
            "HttpResponse",
            "JSONField",
            "pkg.mod",
            "chars()",
            "a::b",
        ] {
            assert!(code_shaped(code), "{code}");
        }
        for word in [
            "autoescape",
            "current_app",
            "Host",
            "Blog",
            "CASCADE",
            "SHA1",
        ] {
            assert!(!code_shaped(word), "{word}");
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg/sub")).unwrap();
        std::fs::write(dir.path().join("pkg/mod.py"), "").unwrap();
        let docs = dir.path().join("docs/ref");
        std::fs::create_dir_all(&docs).unwrap();
        let names = |s: &str| s.split('.').map(str::to_string).collect::<Vec<_>>();
        assert!(names_module(&docs, &names("pkg.sub")));
        assert!(names_module(&docs, &names("pkg.mod")));
        assert!(!names_module(&docs, &names("pkg.mod.func")));
        assert!(!names_module(&docs, &names("pkg")));
    }
}
