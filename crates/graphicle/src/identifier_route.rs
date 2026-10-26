//! The identifier route of `context`, which `recall_project` in the
//! essentials set and `graphicle context` take unless told otherwise, and
//! the HTTP API and the SDKs when a call asks: a question that is an
//! identifier, or names identifiers and asks only where they are defined or
//! used, gets each name's definitions and references from the project's
//! code graph as `path:line` citations, within the call's budget, instead of
//! the full retrieval ([`answer`]). One line names the reviewed memories
//! linked to those definitions ([`crate::symbol_links`]).
//!
//! A name is a backtick span that reads as an identifier, a `snake_case` or
//! `SCREAMING_CASE` word, a camelCase or PascalCase word with an inner
//! capital, a dotted or `::` path, or a call `name()` ([`classify`]). The
//! other words of the question may only ask where the names are or who uses
//! them ([`LOOKUP`]), and at least one must ask that ([`WHERE_OR_USED`]):
//! "where is `parse` defined", "who calls ParseConfig".
//! Every other question keeps the retrieval: one in plain words (a lone
//! plain word too, which reads as prose as often as code), and one that asks
//! more about the names it gives ("how does `parse` handle errors"), whose
//! excerpts the retrieval ranks with the named code. So does a call that
//! names `needs` or turns `graph_retrieval` off, a project without a
//! recorded code graph, and a question none of whose names the code graph
//! defines in the caller's view of the project.
//!
//! The switch is `identifier_route` (left out:
//! [`crate::query::IDENTIFIER_ROUTE_DEFAULT`], off, for `context`;
//! [`crate::api::identifier_route_default`] for `recall_project`); a
//! project packet's `route` says which path ran and why ([`REPORT`]). The
//! route's packet has no excerpts: its `evidence` and `sources` are empty.
use crate::code_graph_arm::{self, Located, Reading};
use crate::store::{Principal, s};
use crate::{Error, Result, graph_projection, integer, string};
use graphicle::graph::Graph;
use graphicle::record::Properties;
use graphicle_memory::graph::span;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

/// The switch, which `recall_project` takes beside its published contract
/// like the other recall switches ([`crate::api::RECALL_SWITCHES`]).
pub const FIELD: &str = "identifier_route";
/// The packet field that says which path ran: `{"path": "identifier"}`, or
/// `{"path": "retrieval", "reason": ...}`.
pub const REPORT: &str = "route";
/// Names one question may look up; a question naming more keeps the
/// retrieval.
pub const MAX_NAMES: usize = 8;
/// Definitions listed per name.
pub const MAX_DEFINITIONS: usize = 20;
/// Titles of linked reviewed memories the text names.
pub const MAX_LINKED_TITLES: usize = 8;
/// The answer's last line before the budget line: how an agent reads on
/// (a terminal leaves it out, [`crate::terminal::context_text`]).
pub const READ_ON: &str =
    "read_project_source reads any of these lines; a question in plain words gets excerpts.";
/// [`READ_ON`] for an answer read outside MCP (`continue_with: "file"`:
/// `graphicle context` and the prompt hook), which names no tool.
pub const READ_ON_FILES: &str =
    "Read these lines in the files; a question in plain words gets excerpts.";

/// The call's switch, and whether the call left it out (then it is
/// [`crate::query::IDENTIFIER_ROUTE_DEFAULT`]).
pub fn switch(args: &Value) -> Result<(bool, bool)> {
    match args.get(FIELD) {
        None | Some(Value::Null) => Ok((crate::query::IDENTIFIER_ROUTE_DEFAULT, true)),
        Some(Value::Bool(on)) => Ok((*on, false)),
        Some(_) => Err(Error::bad("identifier_route must be true or false")),
    }
}

/// What a question asks, for the route.
#[derive(Debug, PartialEq, Eq)]
pub enum Shape {
    /// Names, and words that only ask where they are defined or used.
    Lookup(Vec<String>),
    /// Names, and words that ask more about them.
    AsksMore(Vec<String>),
    /// No name: a question in plain words.
    PlainWords,
}

/// The words a lookup may use beside its names, separated by spaces
/// (compared in lower case).
pub const LOOKUP: &str = "\
    a all an and any are at attribute attributes call called caller callers calling calls \
    can class classes code const constant constants could declaration declarations declare \
    declared declares def define defined defines definition definitions do each enum enums \
    every field fields file files find fn from func function functions get give i identifier \
    identifiers impl implement implementation implementations implemented implements import \
    imported importer importers imports in instance instances interface interfaces \
    invocations invoke invoked invokes is it its line lines list live lives locate located \
    location locations look lookup macro macros me member members method methods module \
    modules name names of on or please properties property ref reference referenced \
    references referencing refs search show site sites source struct structs symbol symbols \
    that the their them these this those to trait traits type types up usage usages use used \
    uses using var variable variables was we were what where which who whom whose";

/// Whether `word` (in lower case) is one of [`LOOKUP`].
fn lookup_word(word: &str) -> bool {
    LOOKUP.split_whitespace().any(|lookup| lookup == word)
}

/// The words of [`LOOKUP`] that ask where a name is or what uses it: a
/// question with words besides its names takes the route only with one of
/// these ("what is `X`", "show me the code of `X`" and "list the fields of
/// `X`" ask for content or members, which the retrieval's excerpts hold).
pub const WHERE_OR_USED: &str = "\
    where who whom whose find locate located location locations lookup define defines \
    defined definition definitions declare declares declared declaration declarations use \
    uses used using usage usages call calls called calling caller callers invoke invokes \
    invoked invocations ref refs reference references referenced referencing import imports \
    imported importer importers implement implements implemented implementations lives site \
    sites";

/// Whether `word` (in lower case) is one of [`WHERE_OR_USED`].
fn where_or_used(word: &str) -> bool {
    WHERE_OR_USED.split_whitespace().any(|usage| usage == word)
}

/// The last segments that make a dotted word a file name, not a symbol.
const FILE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cfg", "cjs", "cpp", "cs", "css", "env", "go", "h", "hpp", "html", "ini", "java",
    "js", "json", "jsx", "kt", "kts", "lock", "md", "mjs", "php", "proto", "py", "pyi", "rb", "rs",
    "scala", "scss", "sh", "sql", "svelte", "swift", "toml", "ts", "tsx", "txt", "vue", "xml",
    "yaml",
];

static BACKTICKS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"`[^`\n]+`").expect("the backtick rule"));
static WORD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\.|#)[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?")
        .expect("the word rule")
});
static IDENTIFIER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\.|#)[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?$")
        .expect("the identifier rule")
});
static CAPITAL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[a-z0-9][A-Z]|[A-Z]{2}[a-z]").expect("the inner capital rule")
});

/// Whether a word outside backticks is a name: a call `name()`, a `::` or
/// `#` path, a dotted path that is not a file name (`parser.py`) or an
/// abbreviation (`e.g`), a word with an underscore, or one with an inner
/// capital (`parseConfig`, `ParseConfig`, `HTTPServer`). A plain word
/// (`parser`, `Config`, `API`) is not.
fn is_name(word: &str) -> bool {
    let bare = word.strip_suffix("()").unwrap_or(word);
    if bare.len() < word.len() || bare.contains("::") || bare.contains('#') {
        return true;
    }
    if bare.contains('.') {
        let segments: Vec<&str> = bare.split('.').collect();
        let last = segments.last().map(|s| s.to_ascii_lowercase());
        return !segments.iter().all(|segment| segment.len() <= 1)
            && !last.is_some_and(|last| FILE_EXTENSIONS.contains(&last.as_str()));
    }
    (bare.contains('_') && bare.chars().any(|c| c.is_ascii_alphanumeric()))
        || CAPITAL.is_match(bare)
}

/// Add `name` once, in the order the question gives.
fn push(names: &mut Vec<String>, name: &str) {
    let name = name.strip_suffix("()").unwrap_or(name).to_string();
    if !names.contains(&name) {
        names.push(name);
    }
}

/// The names `question` gives and what it asks about them ([`Shape`]).
pub fn classify(question: &str) -> Shape {
    let mut names = Vec::new();
    // Backtick spans first: one that reads as an identifier is a name, plain
    // word or not; the words of any other span count as words.
    let mut rest = String::new();
    let mut last = 0;
    for span in BACKTICKS.find_iter(question) {
        rest.push_str(&question[last..span.start()]);
        rest.push(' ');
        last = span.end();
        let inner = span.as_str().trim_matches('`').trim();
        if IDENTIFIER.is_match(inner) {
            push(&mut names, inner);
        } else {
            rest.push_str(inner);
            rest.push(' ');
        }
    }
    rest.push_str(&question[last..]);
    let mut words = String::new();
    let mut last = 0;
    for word in WORD.find_iter(&rest) {
        if is_name(word.as_str()) {
            push(&mut names, word.as_str());
            words.push_str(&rest[last..word.start()]);
            words.push(' ');
            last = word.end();
        }
    }
    words.push_str(&rest[last..]);
    if names.is_empty() {
        return Shape::PlainWords;
    }
    let words: Vec<String> = words
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    // A bare name, or names with words that only ask where they are or what
    // uses them, and at least one word that asks that.
    let lookup = words.iter().all(|word| lookup_word(word))
        && (words.is_empty() || words.iter().any(|word| where_or_used(word)));
    if lookup {
        Shape::Lookup(names)
    } else {
        Shape::AsksMore(names)
    }
}

/// What [`answer`] did: the identifier route's packet, or the `route`
/// report of a call that keeps the retrieval.
#[derive(Debug)]
pub enum Routed {
    Answered(Value),
    Retrieval(Value),
}

/// One reference to a definition: where a relation to it was read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    relpath: String,
    line: Option<usize>,
    relation: String,
    from: Option<String>,
}

/// A string attribute of a node or edge.
fn text(attributes: &impl Properties, key: &str) -> Option<String> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A symbol's dotted name: its `qualified_name`, else its label without a
/// call's `()` and a method's leading `.`.
pub(crate) fn symbol_name(attributes: &impl Properties) -> Option<String> {
    text(attributes, "qualified_name").or_else(|| {
        text(attributes, "label").map(|label| {
            label
                .trim()
                .trim_end_matches("()")
                .trim_start_matches('.')
                .to_string()
        })
    })
}

/// Relations that are not references: containment, and the relations
/// between a rationale or a concept and code.
const NOT_REFERENCES: [&str; 3] = [
    "rationale_for",
    "semantically_similar_to",
    "conceptually_related_to",
];

/// The code symbols of `graph` that `name` defines: those whose dotted name
/// ends with the longest suffix of the name's segments (`Store::open` and
/// `store.Store.open` find `Store.open`); a path's last segment alone counts
/// only in a file whose path names one of the path's other segments, so
/// `os.path.join` does not find every `join`.
fn definitions(symbols: &[(usize, String, String)], name: &str) -> Vec<usize> {
    let dotted = name.replace("::", ".").replace('#', ".");
    let mut segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    while segments.len() > 1 && ["self", "this", "crate", "super"].contains(&segments[0]) {
        segments.remove(0);
    }
    let ends_with = |qualified: &str, path: &str| {
        qualified == path
            || qualified
                .strip_suffix(path)
                .is_some_and(|head| head.ends_with('.'))
    };
    for k in 0..segments.len() {
        let suffix = segments[k..].join(".");
        let last_only = k + 1 == segments.len() && segments.len() > 1;
        let found: Vec<usize> = symbols
            .iter()
            .filter(|(_, qualified, relpath)| {
                ends_with(qualified, &suffix)
                    && (!last_only
                        || relpath
                            .split(['/', '.'])
                            .any(|part| segments[..k].contains(&part)))
            })
            .map(|(index, _, _)| *index)
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// The ends of `edge` as the extractor read them: its `_src` and `_tgt`
/// when they name the edge's two ends (the stored edge may have lost the
/// direction), else the stored ends.
pub(crate) fn reference_ends(graph: &Graph, edge: &graphicle::graph::Edge) -> (usize, usize) {
    let node = |key: &str| edge.attributes.get(key).and_then(|id| graph.node_index(id));
    match (node("_src"), node("_tgt")) {
        (Some(a), Some(b))
            if BTreeSet::from([a, b]) == BTreeSet::from([edge.source, edge.target]) =>
        {
            (a, b)
        }
        _ => (edge.source, edge.target),
    }
}

/// Where an edge says its relation was read: its `source_file` (with `/`
/// separators) and its start line.
pub(crate) fn reference_site(attributes: &impl Properties) -> (Option<String>, Option<usize>) {
    (
        text(attributes, "source_file").map(|path| path.replace('\\', "/")),
        span::start_line(attributes),
    )
}

/// Whether a relation names a reference: not containment, nor a relation
/// between a rationale or a concept and code.
fn refers(relation: &str) -> bool {
    !graphicle::markdown_resolution::CONTAINMENT_RELATIONS.contains(&relation)
        && !NOT_REFERENCES.contains(&relation)
}

/// The reference a relation from `referrer` gives: cited where the relation
/// was read (`site`, the edge's file and line), else at the referrer's own
/// file and line, when `citable` allows that file.
fn reference(
    relation: String,
    site: (Option<String>, Option<usize>),
    referrer: &impl Properties,
    citable: &dyn Fn(&str) -> bool,
) -> Option<Reference> {
    let (relpath, line) = match site {
        (Some(path), Some(line)) => (path, Some(line)),
        _ => (
            code_graph_arm::relpath(referrer)?,
            span::start_line(referrer),
        ),
    };
    if !citable(&relpath) {
        return None;
    }
    let from = code_graph_arm::code_symbol(referrer)
        .then(|| symbol_name(referrer))
        .flatten();
    Some(Reference {
        relpath,
        line,
        relation,
        from,
    })
}

/// The references to node `index`: the other end of every relation that
/// points to it, cited where the relation was read (the edge's file and
/// line, else the referring node's), when `citable` allows that file and,
/// under the lag fallback (`unchanged`), the relation was read from an
/// unchanged file.
fn references(
    graph: &Graph,
    index: usize,
    citable: &dyn Fn(&str) -> bool,
    unchanged: Option<&BTreeSet<String>>,
) -> Vec<Reference> {
    let mut incident = graph.out_edges(index);
    if graph.is_directed() {
        incident.extend(graph.in_edges(index));
    }
    let mut found = BTreeSet::new();
    for (_, _, edge) in incident {
        let (source, target) = reference_ends(graph, edge);
        if target != index || source == index {
            continue;
        }
        let relation = text(&edge.attributes, "relation").unwrap_or_default();
        if !refers(&relation) {
            continue;
        }
        if let Some(unchanged) = unchanged
            && !code_graph_arm::edge_files(&edge.attributes)
                .iter()
                .any(|file| unchanged.contains(file))
        {
            continue;
        }
        let referrer = &graph.nodes()[source].attributes;
        if let Some(reference) = reference(
            relation,
            reference_site(&edge.attributes),
            referrer,
            citable,
        ) {
            found.insert(reference);
        }
    }
    found.into_iter().collect()
}

/// What the route reads of the episode's code graph: the resident graph
/// ([`Resident`]), or the episode's query file ([`Saved`],
/// [`crate::query_file`]), which gives the same answers. A read of the file
/// that fails marks it corrupt ([`crate::query_file::QueryFile::corrupt`]),
/// and [`answer`] then asks the resident graph.
trait Lookup {
    /// The nodes that define `name` ([`definitions`]).
    fn definitions(&self, name: &str) -> Vec<usize>;
    /// Node `index` located in `view` ([`code_graph_arm::locate`]).
    fn locate(&self, index: usize, view: &HashMap<&str, &Value>) -> Option<Located>;
    /// The references to node `index` ([`references`]).
    fn references(
        &self,
        index: usize,
        citable: &dyn Fn(&str) -> bool,
        unchanged: Option<&BTreeSet<String>>,
    ) -> Vec<Reference>;
}

/// The episode's resident graph, with its code symbols and, once read,
/// its symbol start lines.
struct Resident {
    handle: graphicle::session::GraphHandle,
    symbols: Vec<(usize, String, String)>,
    starts: std::sync::OnceLock<HashMap<String, Vec<usize>>>,
}

impl Resident {
    fn new(handle: graphicle::session::GraphHandle) -> Self {
        let graph: &Graph = &handle;
        let symbols: Vec<(usize, String, String)> = graph
            .nodes()
            .iter()
            .enumerate()
            .filter(|(_, node)| code_graph_arm::code_symbol(&node.attributes))
            .filter_map(|(index, node)| {
                Some((
                    index,
                    symbol_name(&node.attributes)?,
                    code_graph_arm::relpath(&node.attributes)?,
                ))
            })
            .collect();
        Self {
            handle,
            symbols,
            starts: std::sync::OnceLock::new(),
        }
    }
}

impl Lookup for Resident {
    fn definitions(&self, name: &str) -> Vec<usize> {
        definitions(&self.symbols, name)
    }
    fn locate(&self, index: usize, view: &HashMap<&str, &Value>) -> Option<Located> {
        let graph: &Graph = &self.handle;
        let starts = self
            .starts
            .get_or_init(|| code_graph_arm::symbol_starts(graph));
        let node_id = match &graph.nodes()[index].id {
            Value::String(id) => id.clone(),
            other => other.to_string(),
        };
        code_graph_arm::locate(graph, starts, &node_id, view)
    }
    fn references(
        &self,
        index: usize,
        citable: &dyn Fn(&str) -> bool,
        unchanged: Option<&BTreeSet<String>>,
    ) -> Vec<Reference> {
        references(&self.handle, index, citable, unchanged)
    }
}

/// The episode's query file.
struct Saved(std::sync::Arc<crate::query_file::QueryFile>);

impl Saved {
    /// [`definitions`] over the code symbols that end in the name's last
    /// segment: the only ones a suffix of the name can end.
    fn definitions_of(
        &self,
        name: &str,
    ) -> std::result::Result<Vec<usize>, crate::query_file::Corrupt> {
        let file = &self.0;
        let dotted = name.replace("::", ".").replace('#', ".");
        let mut segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
        while segments.len() > 1 && ["self", "this", "crate", "super"].contains(&segments[0]) {
            segments.remove(0);
        }
        let Some(last) = segments.last() else {
            return Ok(Vec::new());
        };
        let bucket = file.symbols_ending(last)?;
        let ends_with = |qualified: &str, path: &str| {
            qualified == path
                || qualified
                    .strip_suffix(path)
                    .is_some_and(|head| head.ends_with('.'))
        };
        for k in 0..segments.len() {
            let suffix = segments[k..].join(".");
            let last_only = k + 1 == segments.len() && segments.len() > 1;
            let mut found = Vec::new();
            for &position in &bucket {
                let (index, qualified, relpath, _) = file.symbol(position)?;
                if ends_with(qualified, &suffix)
                    && (!last_only
                        || relpath
                            .split(['/', '.'])
                            .any(|part| segments[..k].contains(&part)))
                {
                    found.push(index);
                }
            }
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Ok(Vec::new())
    }

    fn locate_of(
        &self,
        index: usize,
        view: &HashMap<&str, &Value>,
    ) -> std::result::Result<Option<Located>, crate::query_file::Corrupt> {
        let file = &self.0;
        let attributes = file.attributes(index)?;
        let Some(path) = code_graph_arm::relpath(&attributes) else {
            return Ok(None);
        };
        let starts: HashMap<String, Vec<usize>> = file
            .starts(&path)?
            .map(|lines| HashMap::from([(path, lines)]))
            .unwrap_or_default();
        Ok(code_graph_arm::locate_record(
            file.id(index)?.to_string(),
            &attributes,
            &starts,
            view,
        ))
    }

    fn references_of(
        &self,
        index: usize,
        citable: &dyn Fn(&str) -> bool,
        unchanged: Option<&BTreeSet<String>>,
    ) -> std::result::Result<Vec<Reference>, crate::query_file::Corrupt> {
        let file = &self.0;
        let mut found = BTreeSet::new();
        for id in file.adjacency(index)?.all() {
            let edge = file.edge(id)?;
            if edge.target != index || edge.source == index {
                continue;
            }
            let relation = edge
                .relation
                .as_ref()
                .and_then(Value::as_str)
                .filter(|relation| !relation.is_empty())
                .map(str::to_string)
                .unwrap_or_default();
            if !refers(&relation) {
                continue;
            }
            if let Some(unchanged) = unchanged
                && !edge.files.iter().any(|file| unchanged.contains(file))
            {
                continue;
            }
            let referrer = file.attributes(edge.source)?;
            if let Some(reference) = reference(
                relation,
                (edge.site_file, edge.site_line),
                &referrer,
                citable,
            ) {
                found.insert(reference);
            }
        }
        Ok(found.into_iter().collect())
    }
}

impl Lookup for Saved {
    fn definitions(&self, name: &str) -> Vec<usize> {
        self.definitions_of(name).unwrap_or_default()
    }
    fn locate(&self, index: usize, view: &HashMap<&str, &Value>) -> Option<Located> {
        self.locate_of(index, view).ok().flatten()
    }
    fn references(
        &self,
        index: usize,
        citable: &dyn Fn(&str) -> bool,
        unchanged: Option<&BTreeSet<String>>,
    ) -> Vec<Reference> {
        self.references_of(index, citable, unchanged)
            .unwrap_or_default()
    }
}

/// Each name's definitions (located in the caller's view of the project)
/// and references, and the names whose definitions are all outside that
/// view; `None` when the code graph defines none of the names.
#[allow(clippy::type_complexity)]
fn lookups(
    source: &dyn Lookup,
    db: &Connection,
    repository: &Value,
    p: &Principal,
    names: &[String],
    filter: Option<&crate::query::Filter>,
    unchanged: Option<&BTreeSet<String>>,
) -> Result<Option<(Vec<(String, Vec<Located>, Vec<Reference>)>, Vec<String>)>> {
    // The names' definitions in the code graph first: when it defines none
    // of them, the retrieval answers before any source is read here.
    let candidates: Vec<(&String, Vec<usize>)> = names
        .iter()
        .map(|name| (name, source.definitions(name)))
        .collect();
    if candidates.iter().all(|(_, found)| found.is_empty()) {
        return Ok(None);
    }
    let mut documents = crate::memory::documents(db, repository, p)?;
    if let Some(filter) = filter {
        documents.retain(|document| filter.keeps(document));
    }
    let view: HashMap<&str, &Value> = documents
        .iter()
        .map(|document| (s(document, "id"), document))
        .collect();
    let citable = |path: &str| {
        view.contains_key(format!("file:{path}").as_str())
            && unchanged.is_none_or(|unchanged| unchanged.contains(path))
    };
    let mut found: Vec<(String, Vec<Located>, Vec<Reference>)> = Vec::new();
    let mut missing = Vec::new();
    for (name, indices) in candidates {
        let mut defined: Vec<(usize, Located)> = indices
            .into_iter()
            .filter_map(|index| source.locate(index, &view).map(|located| (index, located)))
            .filter(|(_, located)| citable(&located.relpath))
            .collect();
        if defined.is_empty() {
            missing.push(name.clone());
            continue;
        }
        defined.sort_by(|(_, a), (_, b)| (&a.relpath, a.line).cmp(&(&b.relpath, b.line)));
        let mut used = BTreeSet::new();
        for (index, _) in &defined {
            used.extend(source.references(*index, &citable, unchanged));
        }
        let defined = defined.into_iter().map(|(_, located)| located).collect();
        found.push((name.clone(), defined, used.into_iter().collect()));
    }
    Ok(Some((found, missing)))
}

/// `path:line`, or `path:start-end`, or the path alone.
fn cite(relpath: &str, line: Option<usize>, end: Option<usize>) -> String {
    match (line, end) {
        (Some(line), Some(end)) if end > line => format!("{relpath}:{line}-{end}"),
        (Some(line), _) => format!("{relpath}:{line}"),
        (None, _) => relpath.to_string(),
    }
}

/// The identifier route for one project's `context` call: the packet of
/// definitions and references when the question takes it (see the module
/// documentation), else the `route` report the retrieval's packet carries.
/// `repository` is authorized for `p`; the caller's view of it (with
/// `include` and `metadata`) decides what may be cited.
pub fn answer(db: &Connection, p: &Principal, repository: &Value, args: &Value) -> Result<Routed> {
    let (on, defaulted) = switch(args)?;
    let question = string(args, "question", 16000)?;
    let retrieval = |reason: &str, names: &[String]| -> Result<Routed> {
        Ok(Routed::Retrieval(
            json!({"path":"retrieval","reason":reason,
            FIELD:on,"defaulted":defaulted,"names":names}),
        ))
    };
    if !on {
        return retrieval("off", &[]);
    }
    let names = match classify(question) {
        Shape::PlainWords => return retrieval("plain_words", &[]),
        Shape::AsksMore(names) => return retrieval("asks_more", &names),
        Shape::Lookup(names) => names,
    };
    if names.len() > MAX_NAMES {
        return retrieval("too_many_names", &names);
    }
    if args["needs"]
        .as_array()
        .is_some_and(|needs| !needs.is_empty())
    {
        return retrieval("needs", &names);
    }
    if args["graph_retrieval"] == false {
        return retrieval("graph_retrieval_off", &names);
    }
    let max_tokens = integer(args, "max_tokens", 3000, 16000)?.max(256);
    let max_chars = integer(args, "max_chars", 12000, 64000)?.max(2000);
    let filter = crate::query::Filter::parse(args)?;
    // The code graph the code-graph arm reads; one that cannot be read
    // leaves the question to the retrieval, as a degraded arm never fails
    // a query.
    let id = s(repository, "id");
    let heads = crate::store::heads(db, id)?;
    let current = graph_projection::episode_of(&heads);
    let Some(Reading { episode, lag }) = code_graph_arm::reading(db, id, &heads, &current)
        .ok()
        .flatten()
    else {
        return retrieval("no_code_graph", &names);
    };
    let unchanged = lag.as_ref().map(|lag| &lag.unchanged);
    // The episode's query file when this build wrote one; the resident
    // graph when it has none, or a read of it fails.
    let mut looked = None;
    if let Some(file) = crate::query_file::open(db, id, &episode) {
        let saved = Saved(file.clone());
        let result = crate::phase::phase("route_query_file", || {
            lookups(
                &saved,
                db,
                repository,
                p,
                &names,
                filter.as_ref(),
                unchanged,
            )
        })?;
        file.note("route_query_file_read");
        if !file.corrupt() {
            looked = Some(result);
        }
    }
    let looked = match looked {
        Some(looked) => looked,
        None => {
            let Ok(handle) = graph_projection::resident(db, id, &episode) else {
                return retrieval("no_code_graph", &names);
            };
            lookups(
                &Resident::new(handle),
                db,
                repository,
                p,
                &names,
                filter.as_ref(),
                unchanged,
            )?
        }
    };
    let Some((found, missing)) = looked else {
        return retrieval("not_in_code_graph", &names);
    };
    if found.is_empty() {
        return retrieval("not_in_code_graph", &names);
    }
    // The text, within the budget: the header, then per name its
    // definitions and references while they fit, then the footer.
    let mut markdown = format!(
        "Graphicle code graph of {}: where the names in the question are defined and used, as path:line.\n",
        s(repository, "name")
    );
    let read_on = if args["continue_with"] == "file" {
        READ_ON_FILES
    } else {
        READ_ON
    };
    let footer = |omitted: usize| {
        format!(
            "{read_on}\nBudget: {max_tokens} cl100k_base reference tokens / {max_chars} characters. Omitted: {omitted} definitions and references.\n"
        )
    };
    let reserve = footer(usize::MAX);
    let reserve_tokens = graphicle_memory::text::count(&reserve);
    let reserve_chars = reserve.chars().count();
    let mut tokens = graphicle_memory::text::count(&markdown);
    let mut chars = markdown.chars().count();
    let mut omitted = 0usize;
    let mut fits = |markdown: &mut String, line: String| {
        let (more_tokens, more_chars) =
            (graphicle_memory::text::count(&line), line.chars().count());
        if tokens + more_tokens + reserve_tokens <= max_tokens
            && chars + more_chars + reserve_chars <= max_chars
        {
            tokens += more_tokens;
            chars += more_chars;
            markdown.push_str(&line);
            true
        } else {
            false
        }
    };
    let mut symbols_out = Vec::new();
    for (name, defined, used) in &found {
        let head = format!(
            "`{name}`: {} definition{}, {} reference{}\n",
            defined.len(),
            if defined.len() == 1 { "" } else { "s" },
            used.len(),
            if used.len() == 1 { "" } else { "s" }
        );
        let mut listed_definitions = Vec::new();
        let mut listed_references = Vec::new();
        if fits(&mut markdown, head) {
            for located in defined.iter().take(MAX_DEFINITIONS) {
                let qualified = located.facts["qualified_name"]
                    .as_str()
                    .or_else(|| located.facts["label"].as_str())
                    .unwrap_or(name);
                let line = format!(
                    "- definition {} {qualified}\n",
                    cite(&located.relpath, Some(located.line), Some(located.end_line))
                );
                if !fits(&mut markdown, line) {
                    break;
                }
                listed_definitions.push(json!({"relpath":located.relpath,"line":located.line,
                    "end_line":located.end_line,"qualified_name":located.facts["qualified_name"],
                    "label":located.facts["label"],"stable_id":located.facts["stable_id"]}));
            }
            if listed_definitions.len() == defined.len().min(MAX_DEFINITIONS) {
                for reference in used {
                    let from = reference
                        .from
                        .as_deref()
                        .map(|from| format!(", in {from}"))
                        .unwrap_or_default();
                    let line = format!(
                        "- reference {} {}{from}\n",
                        cite(&reference.relpath, reference.line, None),
                        reference.relation
                    );
                    if !fits(&mut markdown, line) {
                        break;
                    }
                    listed_references.push(json!({"relpath":reference.relpath,
                        "line":reference.line,"relation":reference.relation,"from":reference.from}));
                }
            }
        }
        omitted += defined.len() - listed_definitions.len() + used.len() - listed_references.len();
        symbols_out.push(json!({"name":name,"definitions":listed_definitions,
            "references":listed_references,"definition_count":defined.len(),
            "reference_count":used.len()}));
    }
    // The reviewed memories linked to the definitions (crate::symbol_links),
    // in one line, unless the call leaves memory out.
    let mut linked: Vec<Value> = Vec::new();
    if args["memory_retrieval"] != false {
        let mut nodes: Vec<Value> = found
            .iter()
            .flat_map(|(_, defined, _)| defined.iter())
            .map(|located| json!({"stable_id": located.facts["stable_id"]}))
            .collect();
        crate::symbol_links::attach(db, p, repository, &mut nodes)?;
        let mut seen = BTreeSet::new();
        for memory in nodes
            .iter()
            .flat_map(|node| node["linked"]["memories"].as_array().into_iter().flatten())
        {
            if seen.insert(s(memory, "id").to_string()) {
                linked
                    .push(json!({"id":memory["id"],"kind":memory["kind"],"title":memory["title"]}));
            }
        }
        if !linked.is_empty() {
            let titles: Vec<String> = linked
                .iter()
                .take(MAX_LINKED_TITLES)
                .map(|memory| format!("{} (memory {})", s(memory, "title"), s(memory, "id")))
                .collect();
            let more = linked.len().saturating_sub(MAX_LINKED_TITLES);
            let more = if more > 0 {
                format!("; {more} more")
            } else {
                String::new()
            };
            let line = format!(
                "Reviewed memory linked to these symbols: {}{more}\n",
                titles.join("; ")
            );
            fits(&mut markdown, line);
        }
    }
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|name| format!("`{name}`")).collect();
        let line = format!("Not defined in the code graph: {}\n", names.join(", "));
        fits(&mut markdown, line);
    }
    markdown.push_str(&footer(omitted));
    let mut route = json!({"path":"identifier",FIELD:true,"defaulted":defaulted,
        "names":names,"not_found":missing,"episode":episode});
    if let Some(lag) = &lag {
        route["episode_lag"] = json!(lag.changed);
        route["current_episode"] = json!(lag.current);
    }
    let packet = json!({
        "repository_id": id,
        "question": question,
        "markdown": markdown,
        "characters": markdown.chars().count(),
        "max_chars": max_chars,
        "tokens": graphicle_memory::text::count(&markdown),
        "max_tokens": max_tokens,
        "tokenizer": "cl100k_base (reference; agent tokenization may differ)",
        "symbols": symbols_out,
        "linked_memories": linked,
        "evidence": [],
        "sources": [],
        "memories": [],
        "omitted": {"definitions_and_references": omitted},
        "unresolved_needs": [],
        "complete_evidence_guaranteed": false,
        "model_calls": 0,
        "remote_model_calls": 0,
        "mechanisms": {"switches": crate::query::switches(args, "project")?,
            "graph_episode": episode},
        "code_graph": {"status": "identifier_route", "episode": episode,
            "episode_lag": lag.as_ref().map_or(0, |lag| lag.changed)},
        "freshness": crate::invalidation::repository_freshness(db, &p.workspace, id)?,
        REPORT: route,
    });
    Ok(Routed::Answered(packet))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(names: &[&str]) -> Shape {
        Shape::Lookup(names.iter().map(|name| name.to_string()).collect())
    }

    #[test]
    fn an_identifier_or_a_lookup_of_names_takes_the_route() {
        for (question, names) in [
            ("`parse_config`", vec!["parse_config"]),
            ("parse_config", vec!["parse_config"]),
            ("ParseConfig", vec!["ParseConfig"]),
            ("parseConfig()", vec!["parseConfig"]),
            ("Store::open", vec!["Store::open"]),
            ("os.path.join", vec!["os.path.join"]),
            ("`parser`", vec!["parser"]),
            ("Where is `Store` defined?", vec!["Store"]),
            (
                "who calls parse_config and ParseConfig",
                vec!["parse_config", "ParseConfig"],
            ),
            ("find all references to HTTPServer", vec!["HTTPServer"]),
            ("usages of MAX_NAMES", vec!["MAX_NAMES"]),
        ] {
            assert_eq!(classify(question), lookup(&names), "{question}");
        }
    }

    #[test]
    fn plain_words_and_questions_that_ask_more_keep_the_retrieval() {
        for question in [
            "how does the parser handle errors",
            "parser",
            "Config",
            "where is config.rs",
            "API keys e.g. tokens",
            "`graphicle setup`",
        ] {
            assert_eq!(classify(question), Shape::PlainWords, "{question}");
        }
        for (question, names) in [
            (
                "how does `parse_config` handle a missing file?",
                vec!["parse_config"],
            ),
            ("why is ParseConfig slow", vec!["ParseConfig"]),
            ("where is `Store` defined in `src/store.rs`", vec!["Store"]),
            // Content, members, outgoing calls and mentions, not where a
            // name is or what uses it.
            ("What is MAX_NAMES?", vec!["MAX_NAMES"]),
            ("what is `parse_config` used for", vec!["parse_config"]),
            (
                "what do parse_config and load_config do",
                vec!["parse_config", "load_config"],
            ),
            ("what type is user_id", vec!["user_id"]),
            ("show me the code of parse_config", vec!["parse_config"]),
            (
                "give me the implementation of Store::open",
                vec!["Store::open"],
            ),
            ("list the fields of `RecallArgs`", vec!["RecallArgs"]),
            ("callees of parse_config", vec!["parse_config"]),
            (
                "what functions are called by parse_config",
                vec!["parse_config"],
            ),
            ("where is `RetryPolicy` mentioned", vec!["RetryPolicy"]),
        ] {
            assert_eq!(
                classify(question),
                Shape::AsksMore(names.iter().map(|n| n.to_string()).collect()),
                "{question}"
            );
        }
    }

    #[test]
    fn every_where_or_used_word_is_a_lookup_word() {
        for word in WHERE_OR_USED.split_whitespace() {
            assert!(lookup_word(word), "{word}");
        }
    }

    #[test]
    fn a_path_finds_its_longest_matching_suffix() {
        let symbols = vec![
            (0, "Store.open".to_string(), "src/store.rs".to_string()),
            (1, "open".to_string(), "src/files.rs".to_string()),
            (2, "join".to_string(), "lib/path/util.py".to_string()),
            (3, "join".to_string(), "lib/strings.py".to_string()),
        ];
        assert_eq!(definitions(&symbols, "Store::open"), vec![0]);
        assert_eq!(definitions(&symbols, "crate::store::Store::open"), vec![0]);
        assert_eq!(definitions(&symbols, "self.open"), vec![0, 1]);
        assert_eq!(definitions(&symbols, "open"), vec![0, 1]);
        // A path's last segment alone: only in a file its path names.
        assert_eq!(definitions(&symbols, "os.path.join"), vec![2]);
        assert!(definitions(&symbols, "Reader.join").is_empty());
        assert!(definitions(&symbols, "pen").is_empty());
    }

    #[test]
    fn the_switch_is_a_boolean_left_out_as_the_default() {
        assert_eq!(
            switch(&json!({})).unwrap(),
            (crate::query::IDENTIFIER_ROUTE_DEFAULT, true)
        );
        assert_eq!(
            switch(&json!({"identifier_route":false})).unwrap(),
            (false, false)
        );
        assert!(switch(&json!({"identifier_route":"no"})).is_err());
    }

    /// The query file finds the definitions, locations and references the
    /// resident graph finds, on directed and undirected graphs, with and
    /// without the lag fallback.
    #[test]
    fn the_query_file_answers_lookups_as_the_resident_graph() {
        for directed in [true, false] {
            let dir = tempfile::TempDir::new().unwrap();
            let graph = crate::query_file::tests::sample_of(directed);
            let file = std::sync::Arc::new(crate::query_file::tests::written(&graph, dir.path()));
            let nodes = graph.nodes().len();
            let resident = Resident::new(graphicle::session::GraphHandle::new(graph));
            let saved = Saved(file.clone());
            let mut paths = vec![
                "src/store.rs".to_string(),
                "src/query.rs".to_string(),
                "docs/guide.md".to_string(),
                "src/ünï.rs".to_string(),
            ];
            paths.extend((0..7).map(|i| format!("src/helpers/h{i}.rs")));
            let content: String = (1..=120).map(|i| format!("line {i}\n")).collect();
            let documents: Vec<Value> = paths
                .iter()
                .map(|path| json!({"id": format!("file:{path}"), "content": content}))
                .collect();
            let view: HashMap<&str, &Value> = documents
                .iter()
                .map(|document| (s(document, "id"), document))
                .collect();
            let citable = |path: &str| view.contains_key(format!("file:{path}").as_str());
            let unchanged = BTreeSet::from(["src/query.rs".to_string()]);
            for name in [
                "Store.open",
                "store.Store.close",
                "Store::open",
                "Store#open",
                "run_query",
                "query.run_query",
                "plan",
                "helper_3",
                "Store",
                "self.open",
                "missing",
                "",
            ] {
                let found = resident.definitions(name);
                assert_eq!(found, saved.definitions(name), "{name}");
                for index in found {
                    assert_eq!(
                        format!("{:?}", resident.locate(index, &view)),
                        format!("{:?}", saved.locate(index, &view)),
                        "{name}"
                    );
                }
            }
            for index in 0..nodes {
                for lag in [None, Some(&unchanged)] {
                    assert_eq!(
                        resident.references(index, &citable, lag),
                        saved.references(index, &citable, lag),
                        "node {index}"
                    );
                }
            }
            assert!(!file.corrupt());
        }
    }
}
