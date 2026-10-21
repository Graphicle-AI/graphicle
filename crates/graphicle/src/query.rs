use crate::memory;
use crate::store::{Principal, Store, decode, one, repo, rows, s};
use crate::{Error, Result, digest, graph_projection, integer, native, string};
use graphicle::graph::Graph;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Default fusion weight of the dense ("semantic") arm; every other arm
/// counts 1. It was 2 (chosen against weight 1 on LongMemEval-S with the
/// bundled MiniLM embedder) and is 0.5: on the development sets (BEAM-100K
/// and LongMemEval-S) weight 0.5 recalled more of the evidence than weight 2,
/// with slightly lower session recall on LongMemEval-S, and on held-out
/// BEAM-100K units weight 0.5 ties stemmed BM25 alone (weight 2 was not run
/// there).
///
/// So 0.5 is the less harmful dense weight; it is not shown to beat lexical
/// retrieval on held-out data.
pub const SEMANTIC_WEIGHT: f64 = 0.5;

/// Deepest cross-encoder candidate list a call may ask for (`rerank_depth`).
/// Hindsight reranks up to 300 candidates and SmartSearch (arXiv 2603.15599)
/// 400+; beyond the core's `MAX_RANK_LIMIT` the list continues down the fused
/// order.
pub const MAX_RERANK_DEPTH: usize = 300;

/// `graphicle context`'s default `max_chars`. A UserPromptSubmit hook
/// prints the packet as JSON (`additionalContext`) under a short header, and
/// Claude Code (2.1.287) shows the model only a preview of hook output over
/// 10,000 characters. 0.1.1's default (4 characters per `max_tokens` token,
/// 12,000 at 3,000 tokens) could go over; at 8,400 characters the escaped
/// JSON, header included, stays below 9,500.
pub const PUSH_CHARS: usize = 8_400;

/// `notes_first` when a call leaves it out: off, so `context` (and so
/// `recall_project`, `graphicle context` and answers) builds 0.1.2's packet.
/// `notes_first: true` turns on, for one call, the notes-first order of a
/// question in prose and one window per file ([`crate::notes_first`]), and
/// dated excerpts ([`crate::source_age`], which `source_age` also turns on or
/// off by itself). The packet's `notes_first` says what applied.
pub const NOTES_FIRST_DEFAULT: bool = false;

/// `identifier_route` when a call to `context` leaves it out: off, so the
/// HTTP API, `graphicle call context`, the SDKs and the app's evidence
/// button keep the retrieval unless a call asks for the route
/// (`identifier_route: true`). `recall_project` in the essentials set
/// ([`crate::api::identifier_route_default`]) and `graphicle context` ask
/// for it unless told otherwise: a question that is an identifier, or only
/// asks where named identifiers are defined or used, then gets their
/// definitions and references from the code graph instead of the full
/// retrieval ([`crate::identifier_route`]). The packet's `route` says which
/// path ran.
pub const IDENTIFIER_ROUTE_DEFAULT: bool = false;

/// Sources `rank` offers past a call's `limit`, so the windows that
/// [`crate::notes_first::one_per_file`] drops are replaced.
pub const EXTRA_SOURCES: usize = 4;

/// Passages of the fused order `rank` returns for a question in prose, from
/// which [`crate::notes_first::rerank`] takes its first files.
pub const NOTES_FUSED: usize = 400;

/// `more_files` when a call leaves it out, for a code question
/// ([`crate::code_question::is_code_question`]); other questions list none.
/// After its excerpts a packet lists the paths of this many files that rank
/// next (one line each, from at most an eighth of the budget), so an agent
/// sees the files the budget cut.
pub const MORE_FILES_DEFAULT: usize = 10;

/// Passages of the fused order `rank` returns for the files a packet lists
/// after its excerpts.
pub const MORE_FILES_FUSED: usize = 200;

/// Retrieval options that are on unless a call turns them off. An absent or
/// `null` option takes its value here and is listed in the packet's
/// `retrieval_defaults`; `false` turns it off. Porter-stemmed BM25 (Lucene's
/// English analyzer; Mem0 lemmatizes) with the speaker arm routed by the
/// question raised all-sessions recall significantly on LongMemEval S, at
/// this API's default budget and at 8,000 tokens, and on M at 8,000 tokens.
/// Focus windows left recall unchanged on S and M and raised evidence hits on
/// the code questions, not significantly. The per-origin quota gradient stays opt-in
/// (`origin_quota: true`): it lowered all-sessions recall on S for a code gain
/// that was not significant. Runs: tools/retrieval_recall.py; README
/// "Retrieval defaults".
pub fn recipe_defaults() -> Value {
    json!({"stem":true,"speaker":"auto","pack_focus":true})
}

/// Where `context` (and so `recall_project`) presents excerpts of test
/// files ([`test_file`]), the `include_tests` option: `true` keeps 0.1.0's
/// order; `false` presents every test-file excerpt after the other
/// excerpts; `"auto"` (the default) does that unless the question names a
/// test file or a test identifier ([`question_names_tests`]). Only the
/// presentation moves: the candidate bound and the budget still go to the
/// windows in rank order, so the packet holds the same excerpts as with
/// `true`, and each group keeps its order. The packet's `test_files`
/// reports what applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncludeTests {
    /// `true`: test files rank like other files (0.1.0).
    Yes,
    /// `false`: test files always follow the other files.
    No,
    /// `"auto"`: test files follow the other files unless the question
    /// names a test file or a test identifier.
    Auto,
}

/// `include_tests` when a call leaves it out: `"auto"`. Pillar 2's
/// registered offline benchmark (E1) measured this rule, applied to the
/// files of 0.1.0's default packets in delivered order, on SWE-bench
/// Verified's 400 instances that study 1 did not use, with the issue text
/// as the question: test files last raised file recall@1 from 0.245 to
/// 0.310 (+0.065, 95% CI [+0.043, +0.090]; 29 tasks up, 0 down) and left
/// recall@10 at 0.544; with [`question_names_tests`]'s exception, which
/// fired on 55 of the 400 issue texts, recall@1 was 0.299 (+0.054
/// [+0.034, +0.076]; 24 up, 0 down). Both met the registered rule (at
/// least +0.05 with the CI above 0, recall@10 not lower). On 45 of those
/// tasks, the packets this code builds gave E1's own file lists for both
/// rules (44 of 45 identical in their first 10 files, the 45th differing
/// in the base packet alike; every metric equal to 3 decimals). SWE-bench's
/// gold never holds a test file, so what the move costs a question about
/// tests was not measured; `true` keeps 0.1.0's order.
pub const INCLUDE_TESTS_DEFAULT: IncludeTests = IncludeTests::Auto;

impl IncludeTests {
    /// The call's `include_tests` (absent or `null`: the default).
    pub fn parse(args: &Value) -> Result<(Self, bool)> {
        match args.get("include_tests") {
            None | Some(Value::Null) => Ok((INCLUDE_TESTS_DEFAULT, true)),
            Some(Value::Bool(true)) => Ok((Self::Yes, false)),
            Some(Value::Bool(false)) => Ok((Self::No, false)),
            Some(Value::String(mode)) if mode == "auto" => Ok((Self::Auto, false)),
            Some(_) => Err(Error::bad("include_tests must be true, false or auto")),
        }
    }
    fn value(self) -> Value {
        match self {
            Self::Yes => json!(true),
            Self::No => json!(false),
            Self::Auto => json!("auto"),
        }
    }
}

/// Whether a window comes from a test file of the project: a `file:` source
/// whose path has a `test`, `tests` or `testing` folder, or whose file is
/// `test_*`, `*_test.py`, `*_tests.py` or `conftest.py`. This is the TEST
/// rule of the registered benchmark's rankers (see
/// [`INCLUDE_TESTS_DEFAULT`]), character for character, so the rule that
/// ships is the one measured. Other languages' test-file names
/// (`header_test.go`, `parser.spec.ts`, `__tests__/`) are not in it.
pub fn test_file(window: &Value) -> bool {
    static TEST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(^|/)(tests?|testing)/|(^|/)test_[^/]*$|_tests?\.py$|(^|/)conftest\.py$",
        )
        .expect("the test-file rule")
    });
    let source = &window["source"];
    s(source, "id").starts_with("file:") && TEST.is_match(&s(source, "path").replace('\\', "/"))
}

/// Whether `question` names a test file or a test identifier: a path
/// through a `test`, `tests` or `testing` folder (`tests/test_parser.py`,
/// `django/test/utils.py`), a `test_` name (`test_parse_header`,
/// `test_parser.py`), a `_test.py` or `_tests.py` file, `conftest.py`, or a
/// `Test` class (`TestHTTPParser`). The words test, tests or testing alone
/// do not count ("which tests cover parse_header"), nor do pytest, unittest
/// or other names that only contain them. This is the rule of a ranker in the
/// registered benchmark (see [`INCLUDE_TESTS_DEFAULT`]), so the exception
/// that ships is the one measured. Test-file names of other languages
/// (`header_test.go`, `parser.spec.ts`) are not in it; `include_tests: true`
/// keeps test files in place for any question.
pub fn question_names_tests(question: &str) -> bool {
    static NAMES_TESTS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?:^|[^A-Za-z0-9_])(?:tests?|testing)/[A-Za-z0-9_./-]",
            r"|(?:^|[^A-Za-z0-9_])test_[A-Za-z0-9_]+",
            r"|[A-Za-z0-9_]_tests?\.py\b|\bconftest\.py\b",
            r"|\bTest[A-Z][A-Za-z0-9_]*",
        ))
        .expect("the names-tests rule")
    });
    NAMES_TESTS.is_match(question)
}

/// Mark the test-file windows among the packer's `hits` to be presented
/// after the others (the packer's `last`); how many were marked.
fn mark_test_files_last(hits: &mut [Value]) -> usize {
    let mut marked = 0;
    for hit in hits.iter_mut().filter(|hit| test_file(hit)) {
        hit["last"] = json!(true);
        marked += 1;
    }
    marked
}

/// The argument, beside the published contracts of `recall_project` and
/// the graph tools, that asks for the whole answer in `structuredContent`:
/// the reply 0.1.0 sent ([`lean_reply`] has the default).
pub const STRUCTURED: &str = "structured";

/// Most characters of `recall_project`'s Markdown over MCP: `context`'s
/// default `max_chars`, a quarter of the 50,000 characters above which
/// Claude Code hands the model a file pointer instead of a tool's text.
pub const MCP_RECALL_CHARS: u64 = 12_000;

/// Take [`STRUCTURED`] out of the arguments of `recall_project` or a graph
/// tool, before its contract is checked: `true` asks for the 0.1.0 reply.
/// Other tools keep it, and their contract refuses it.
pub fn take_structured(name: &str, graph_tool: bool, args: &mut Value) -> Result<bool> {
    if name != "recall_project" && !graph_tool {
        return Ok(false);
    }
    match args.as_object_mut().and_then(|a| a.remove(STRUCTURED)) {
        None => Ok(false),
        Some(Value::Bool(structured)) => Ok(structured),
        Some(_) => Err(Error::bad(format!("{STRUCTURED} must be true or false"))),
    }
}

/// The line [`lean_reply`] adds for the `needs` a packet's excerpts do not
/// cover.
fn needs_line(needs: &[&str]) -> String {
    format!(
        "Needs these excerpts do not cover (a lexical check): {}\n",
        needs.join("; ")
    )
}

/// `recall_project`'s arguments for the default reply: `max_chars` at most
/// [`MCP_RECALL_CHARS`], less the room the needs line takes when the call
/// names `needs` (the line for every one of them; at least 2,000 characters
/// stay for the packet), so the packet's Markdown, whose budget line then
/// says so, and that line fit the reply.
pub fn cap_recall(args: &mut Value) {
    let reserve = {
        let needs: Vec<&str> = args["needs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if needs.is_empty() {
            0
        } else {
            needs_line(&needs).chars().count() as u64
        }
    };
    let cap = MCP_RECALL_CHARS.saturating_sub(reserve).max(2000);
    // `context`'s default `max_chars` is MCP_RECALL_CHARS.
    let asked = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(MCP_RECALL_CHARS);
    if asked > cap {
        args["max_chars"] = json!(cap);
    }
}

/// The default MCP reply of `recall_project` and the graph tools: the text
/// an agent reads, once. Claude Code (2.1.287) shows the model a tool's
/// `structuredContent` as JSON whenever a reply has one, instead of its
/// `content`; 0.1.0 sent the whole packet there and again as escaped JSON
/// text (recall: 40,000 to 56,000 characters on study 1's repositories;
/// graph tools: a median of 55 KB). So the reply has no
/// `structuredContent`:
/// - `recall_project`: the packet's Markdown, which cites every excerpt
///   (path and line, source id, SHA-256, where to continue), and the needs
///   it did not find, at most [`MCP_RECALL_CHARS`] characters;
/// - a graph tool: the engine's text, then the citations block
///   ([`crate::graph_tools::citations_text`]).
///
/// `None` (the 0.1.0 reply stands) for any other tool or an answer without
/// that text. [`STRUCTURED`] asks for the 0.1.0 reply.
pub fn lean_reply(name: &str, graph_tool: bool, value: &Value) -> Option<Value> {
    let content = if graph_tool {
        let mut content = vec![json!({"type":"text","text":value.get("text")?.as_str()?})];
        if value["nodes"].is_array() {
            let citations = crate::graph_tools::citations_text(value).replace(
                " in structuredContent",
                " not listed (structured: true lists every citation)",
            );
            content.push(json!({"type":"text","text":citations}));
        }
        content
    } else if name == "recall_project" {
        let mut text = value.get("markdown")?.as_str()?.to_string();
        let needs: Vec<&str> = value["unresolved_needs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let line = needs_line(&needs);
        // cap_recall reserved room for it; only needs longer than 10,000
        // characters in all can leave it out.
        if !needs.is_empty()
            && (text.chars().count() + line.chars().count()) as u64 <= MCP_RECALL_CHARS
        {
            text.push_str(&line);
        }
        vec![json!({"type":"text","text":text})]
    } else {
        return None;
    };
    Some(json!({"content":content,"isError":false}))
}

#[derive(Clone)]
pub struct Snapshot {
    pub repository: Value,
    pub documents: Vec<Value>,
    pub memories: Vec<Value>,
    pub graph: Value,
    pub state: String,
}
/// Legacy file-relationship graph derived from the per-file `repo_parses`
/// analyses through `structure::graph`. Superseded by the projected code-graph
/// episode (`episode_graph`); retained only as the fallback for repositories
/// without a recorded episode and for the B10 equivalence test.
pub fn legacy_graph(db: &Connection, repository: &Value, _p: &Principal) -> Result<Value> {
    // Past the adapter's file limit (a project larger than earlier builds
    // allowed), file relationships come from the projected episode only.
    let files: i64 = db.query_row(
        "SELECT count(*) FROM repo_heads WHERE repository_id=?",
        [s(repository, "id")],
        |r| r.get(0),
    )?;
    if usize::try_from(files).unwrap_or(usize::MAX) > graphicle_memory::structure::GRAPH_FILE_LIMIT
    {
        return Ok(
            json!({"edges":[],"unresolved":[],"symbol_calls":[],"unresolved_calls":[],
            "coverage":{"files":files,"symbols":0,"incomplete_files":[],"incomplete":[]},
            "scope":"File relationships of a project this large come from its code graph, which is not built yet",
            "revision":graphicle_memory::structure::REVISION,"graph_source":"legacy","limited":"files"}),
        );
    }
    let mut analyses = BTreeMap::new();
    for mut row in rows(
        db,
        "SELECT p.path,p.data FROM repo_parses p JOIN repo_heads h ON h.repository_id=p.repository_id AND h.path=p.path AND h.sha256=p.sha256 WHERE p.repository_id=? ORDER BY p.path",
        [s(repository, "id")],
    )? {
        decode(&mut row, "data")?;
        analyses.insert(s(&row, "path").to_string(), row["data"].clone());
    }
    let mut graph = native("graph", json!({"analyses":analyses}))?;
    graph["graph_source"] = json!("legacy");
    Ok(graph)
}
/// Basis recorded on every edge derived from a projected episode.
pub const EPISODE_BASIS: &str = "projected_graph_episode";

fn node_text<'a>(attributes: &'a impl graphicle::record::Properties, key: &str) -> Option<&'a str> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}
/// `L5` / `L5-L9` → 5; the episode's `span.start_line` when present.
fn node_line(attributes: &impl graphicle::record::Properties) -> Option<u64> {
    if let Some(start) = attributes
        .get("span")
        .and_then(|span| span.get("start_line"))
        .and_then(Value::as_u64)
    {
        return Some(start);
    }
    node_text(attributes, "source_location")?
        .strip_prefix('L')?
        .split(['-', ':'])
        .next()?
        .trim()
        .parse()
        .ok()
}
/// File-level relationship view of a projected code-graph episode, in the shape
/// every consumer of [`Snapshot::graph`] expects: `edges` between repository
/// relative paths (`imports`, `calls`, `references`, ...), `symbol_calls`
/// between named symbols, and `coverage`. Documented differences from the
/// legacy adapter (see `tests/it/query_graph_source.rs`):
/// - `imports` and `imports_from` both map to `kind: "imports"`; containment
///   relations (`contains`, `method`) never become edges.
/// - Cross-file `calls` appear as file edges (the legacy adapter kept them in
///   `symbol_calls` only) with `basis: "projected_graph_episode"`.
/// - Markdown `document_link` edges are legacy-only; the episode records
///   document nodes and `references` edges instead.
/// - `coverage.symbols` counts non-file nodes anchored to a file; `incomplete`
///   is always empty because the episode does not carry parser issues.
pub fn episode_graph(graph: &Graph, episode: &str) -> Value {
    let nodes = graph.nodes();
    let relpath = |index: usize| {
        let attributes = &nodes[index].attributes;
        node_text(attributes, "relpath")
            .or_else(|| node_text(attributes, "source_file"))
            .map(|p| p.replace('\\', "/"))
    };
    let name = |index: usize| {
        let attributes = &nodes[index].attributes;
        node_text(attributes, "qualified_name")
            .or_else(|| node_text(attributes, "label"))
            .unwrap_or("")
            .to_string()
    };
    let is_file = |index: usize| node_text(&nodes[index].attributes, "kind") == Some("file");
    let mut edges = Vec::new();
    let mut symbol_calls = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_calls = BTreeSet::new();
    for (source, target, edge) in graph.iter_edges() {
        let relation = node_text(&edge.attributes, "relation").unwrap_or("");
        if relation.is_empty()
            || graphicle::markdown_resolution::CONTAINMENT_RELATIONS.contains(&relation)
        {
            continue;
        }
        let (Some(from), Some(to)) = (relpath(source), relpath(target)) else {
            continue;
        };
        let line = edge
            .attributes
            .get("source_location")
            .and_then(Value::as_str)
            .and_then(|l| l.strip_prefix('L'))
            .and_then(|l| l.split(['-', ':']).next()?.trim().parse::<u64>().ok())
            .or_else(|| node_line(&nodes[source].attributes))
            .unwrap_or(1);
        if relation == "calls" && !is_file(source) && !is_file(target) {
            let record = (from.clone(), name(source), to.clone(), name(target), line);
            if seen_calls.insert(record) {
                symbol_calls.push(json!({"path":from,"caller":name(source),"callee_path":to,"callee":name(target),"line":line,
                    "basis":EPISODE_BASIS,"derivation":edge.attributes.get("derivation").cloned().unwrap_or(Value::Null),"runtime_verified":false}));
            }
        }
        if from == to {
            continue;
        }
        let kind = match relation {
            "imports" | "imports_from" => "imports",
            other => other,
        };
        if seen.insert((from.clone(), to.clone(), line, kind.to_string())) {
            edges.push(
                json!({"source":from,"target":to,"line":line,"kind":kind,"basis":EPISODE_BASIS,
                "module":name(target),"relation":relation,
                "derivation":edge.attributes.get("derivation").cloned().unwrap_or(Value::Null),
                "confidence":edge.attributes.get("confidence").cloned().unwrap_or(Value::Null)}),
            );
        }
    }
    let files: BTreeSet<String> = (0..nodes.len()).filter_map(relpath).collect();
    let symbols = (0..nodes.len())
        .filter(|&i| !is_file(i) && relpath(i).is_some())
        .count();
    json!({"edges":edges,"unresolved":[],"symbol_calls":symbol_calls,"unresolved_calls":[],
        "coverage":{"files":files.len(),"symbols":symbols,"incomplete_files":[],"incomplete":[]},
        "scope":"Static source relationships in the projected code-graph episode; not a runtime call graph",
        "revision":graphicle_memory::structure::REVISION,"graph_source":"episode","episode":episode,
        "graph_nodes":nodes.len(),"graph_edges":graph.edge_count()})
}
/// The projected current episode's file-relationship view when the episode
/// is recorded, else the legacy adapter. A recorded episode that cannot be
/// reopened degrades to the legacy adapter with the error reported under
/// `episode_error`, never a failed query.
pub fn source_graph(db: &Connection, repository: &Value, p: &Principal) -> Result<Value> {
    let id = s(repository, "id");
    let episode = graph_projection::current_episode(db, id)?;
    if !graph_projection::is_recorded(db, id, &episode)? {
        return legacy_graph(db, repository, p);
    }
    match graph_projection::resident(db, id, &episode) {
        Ok(handle) => Ok(episode_graph(&handle, &episode)),
        Err(error) => {
            let mut graph = legacy_graph(db, repository, p)?;
            graph["episode"] = json!(episode);
            graph["episode_error"] = json!(error.message);
            Ok(graph)
        }
    }
}
pub fn snapshot(db: &Connection, repository: &Value, p: &Principal) -> Result<Snapshot> {
    let documents = memory::documents(db, repository, p)?;
    // One read of the sources serves the memory listing too.
    let memories = memory::list_of(db, repository, &documents, true)?;
    let graph = source_graph(db, repository, p)?;
    let state = digest(
        &json!({"sources":memory::state(&documents),"memories":memories.iter().map(|m|json!([m["id"],m["revision"],m["state"],m["stale"]])).collect::<Vec<_>>(),"reviewed_state":memory::reviewed_state_for(db,repository,&memories)?}),
    );
    Ok(Snapshot {
        repository: repository.clone(),
        documents,
        memories,
        graph,
        state,
    })
}
/// Validated query filters: `include` (1–32 source-id prefixes, each 1–300
/// bytes, e.g. `connector:<id>:` or `file:docs/`) and `metadata` (at most 16
/// exact values of a document's metadata, e.g. an SDK's `{"agent_id":"a"}`).
pub struct Filter {
    include: Option<Vec<String>>,
    metadata: Option<serde_json::Map<String, Value>>,
}

impl Filter {
    pub fn parse(args: &Value) -> Result<Option<Self>> {
        let include = match args.get("include") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_array()
                    .filter(|v| !v.is_empty() && v.len() <= 32)
                    .ok_or_else(|| Error::bad("include must list 1–32 source ID prefixes"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|p| !p.trim().is_empty() && p.len() <= 300)
                            .map(str::to_string)
                            .ok_or_else(|| Error::bad("Each include prefix must be 1–300 bytes"))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let metadata = match args.get("metadata") {
            None | Some(Value::Null) => None,
            Some(Value::Object(fields)) if !fields.is_empty() && fields.len() <= 16 => {
                Some(fields.clone())
            }
            _ => {
                return Err(Error::bad(
                    "metadata must be an object of 1–16 exact values",
                ));
            }
        };
        Ok((include.is_some() || metadata.is_some()).then_some(Self { include, metadata }))
    }
    pub(crate) fn keeps(&self, document: &Value) -> bool {
        let id = s(document, "id");
        self.include
            .as_ref()
            .is_none_or(|prefixes| prefixes.iter().any(|p| id.starts_with(p.as_str())))
            && self.metadata.as_ref().is_none_or(|fields| {
                fields
                    .iter()
                    .all(|(key, value)| document["metadata"].get(key) == Some(value))
            })
    }
}

/// Sources a memory's data cites: repository files, workspace links and
/// connector documents.
fn cited_sources(data: &Value) -> BTreeSet<String> {
    let mut cited = BTreeSet::new();
    for (field, prefix) in [
        ("dependencies", "file:"),
        ("evidence_links", ""),
        ("native_sources", ""),
    ] {
        if let Some(entries) = data[field].as_object() {
            cited.extend(entries.keys().map(|key| format!("{prefix}{key}")));
        }
    }
    cited
}

/// Search only the sources a filter keeps. Memories are kept only when every
/// source they cite is kept (a memory citing none is left out), so nothing
/// derived from an excluded source is ranked. The snapshot key covers the
/// filter. Returns the filter report.
pub fn restrict(snapshot: &mut Snapshot, filter: &Filter) -> Value {
    let before = snapshot.documents.len();
    snapshot.documents.retain(|d| filter.keeps(d));
    let kept: BTreeSet<&str> = snapshot.documents.iter().map(|d| s(d, "id")).collect();
    let memories = snapshot.memories.len();
    snapshot.memories.retain(|m| {
        let cited = cited_sources(&m["data"]);
        !cited.is_empty() && cited.iter().all(|c| kept.contains(c.as_str()))
    });
    let report = json!({"include":filter.include,"metadata":filter.metadata,
        "documents_searched":snapshot.documents.len(),"documents_excluded":before - snapshot.documents.len(),
        "memories_excluded":memories - snapshot.memories.len()});
    snapshot.state = digest(&json!({"base":snapshot.state,"filter":report}));
    report
}

pub fn candidates(snapshot: &Snapshot) -> Result<(Vec<Value>, Value)> {
    candidates_from(snapshot, passages(snapshot)?, snapshot.memories.iter())
}

/// One chunking of a snapshot's documents: its passage rows and what the
/// memory crate's `chunks` reported. Every ranking of one query over the same
/// snapshot (the query, `graph_local`'s seed and walk, the dense arm) can
/// share it, so the documents are hash-checked and chunked once.
#[derive(Clone)]
pub struct Passages {
    pub rows: Vec<Value>,
    report: Value,
}

/// The snapshot's passages. When the catalogue exceeds the passage bound,
/// the newest documents are searched (equal times: the latest listed, so the
/// latest session files of a synced folder) and the documents left out are
/// counted and described as the memory crate's `chunks` found them
/// (`omitted_documents_are`: none, oldest or earliest_listed). Past the
/// document bound the oldest are left out unread (`document_limit_reached`).
pub fn passages(snapshot: &Snapshot) -> Result<Passages> {
    let mut request = json!({"scope":snapshot.repository["id"],"documents":[],"max_passages":graphicle_memory::ingest::MAX_PASSAGES,"characters":2600,"tokens":640,"prefer":"newest"});
    // A plain copy: `json!` would serialize the document bodies one by one.
    request["documents"] = Value::Array(snapshot.documents.clone());
    let mut report = native("chunks", request)?;
    let rows = match report.get_mut("rows").map(Value::take) {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    };
    Ok(Passages { rows, report })
}

/// The passages one question reads. Within the per-question bounds
/// ([`graphicle_memory::ingest::MAX_DOCUMENTS`] documents and
/// [`graphicle_memory::ingest::MAX_PASSAGES`] passages) this is
/// [`passages`]. Past them, which documents are read is decided by the
/// question: the documents are ordered by a BM25 score of the question's
/// words in each (a word in the path or title counts twice more), so the
/// best are taken first among documents of equal time, and the coverage
/// says so (`selected_by_question`), instead of leaving out the earliest
/// listed paths of a large project. The score divides by length as BM25
/// does: a long file that holds many of the words no longer fills the bound
/// before a short one that holds the rare ones.
pub fn passages_for(snapshot: &mut Snapshot, question: &str) -> Result<Passages> {
    if snapshot.documents.len() <= graphicle_memory::ingest::MAX_DOCUMENTS {
        let within = passages(snapshot)?;
        if within.report["passage_limit_reached"] != true
            && within.report["document_limit_reached"] != true
        {
            return Ok(within);
        }
    }
    if !order_by_question(&mut snapshot.documents, question) {
        return passages(snapshot);
    }
    let mut selected = passages(snapshot)?;
    selected.report["selected_by_question"] = json!(true);
    Ok(selected)
}

/// Most words of a question that choose the documents read past the bounds.
const SELECTION_TERMS: usize = 64;

/// The words of a question that select documents: letters, digits and `_`,
/// at least three characters, without common words, each once, at most
/// [`SELECTION_TERMS`]: words written as code (with `_`, a digit or a
/// capital inside) first, then the others in the question's order. The
/// scores weigh each by how few documents hold it.
fn question_terms(question: &str) -> Vec<String> {
    const COMMON: [&str; 40] = [
        "the", "and", "for", "with", "what", "which", "where", "when", "does", "how", "why", "who",
        "that", "this", "from", "into", "are", "was", "were", "has", "have", "had", "not", "can",
        "about", "there", "their", "they", "them", "then", "than", "its", "our", "your", "you",
        "all", "any", "did", "should", "would",
    ];
    let coded = |word: &str| {
        word.contains('_')
            || word.chars().any(|c| c.is_ascii_digit())
            || word.chars().skip(1).any(char::is_uppercase)
    };
    let mut code = Vec::new();
    let mut plain = Vec::new();
    for word in question.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let lower = word.to_lowercase();
        if lower.chars().count() < 3
            || COMMON.contains(&lower.as_str())
            || code.contains(&lower)
            || plain.contains(&lower)
        {
            continue;
        }
        if coded(word) {
            code.push(lower);
        } else {
            plain.push(lower);
        }
    }
    code.extend(plain);
    code.truncate(SELECTION_TERMS);
    code
}

/// Order `documents` by a BM25 score (k1 1.2, b 0.75, case-insensitive
/// matches of the question's words in the content, each word's weight by
/// how few documents hold it, plus twice the weight of a word in the path
/// or title), lowest first: the chunker takes documents of equal time from
/// the end of the list. Stable, so documents that score equally keep their
/// order. False when the question has no words to select by.
fn order_by_question(documents: &mut Vec<Value>, question: &str) -> bool {
    let mut terms = question_terms(question);
    if terms.is_empty() {
        return false;
    }
    // Longest first, so `parse_config` is counted before `config`.
    terms.sort_by_key(|term| std::cmp::Reverse(term.len()));
    let pattern: Vec<String> = terms.iter().map(|term| regex::escape(term)).collect();
    let Ok(matcher) = regex::RegexBuilder::new(&format!("(?:{})", pattern.join("|")))
        .case_insensitive(true)
        .build()
    else {
        return false;
    };
    let index: std::collections::HashMap<&str, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, term)| (term.as_str(), i))
        .collect();
    // Per document: each word's count in the content, the words of its
    // path or title, and its length.
    let counted: Vec<(Vec<u32>, BTreeSet<usize>, usize)> = documents
        .iter()
        .map(|document| {
            let mut counts = vec![0u32; terms.len()];
            let content = s(document, "content");
            for hit in matcher.find_iter(content) {
                if let Some(&i) = index.get(hit.as_str().to_lowercase().as_str()) {
                    counts[i] += 1;
                }
            }
            let named = format!("{}\n{}", s(document, "path"), s(document, "title"));
            let names = matcher
                .find_iter(&named)
                .filter_map(|hit| index.get(hit.as_str().to_lowercase().as_str()).copied())
                .collect();
            (counts, names, content.len())
        })
        .collect();
    let n = counted.len() as f64;
    let average =
        (counted.iter().map(|(_, _, length)| *length).sum::<usize>() as f64 / n.max(1.0)).max(1.0);
    let weights: Vec<f64> = (0..terms.len())
        .map(|i| {
            let held = counted
                .iter()
                .filter(|(counts, names, _)| counts[i] > 0 || names.contains(&i))
                .count() as f64;
            (1.0 + (n - held + 0.5) / (held + 0.5)).ln()
        })
        .collect();
    let mut scored: Vec<(f64, Value)> = documents
        .drain(..)
        .zip(counted)
        .map(|(document, (counts, names, length))| {
            let norm = 1.2 * (0.25 + 0.75 * length as f64 / average);
            let mut score = 0.0;
            for (i, &weight) in weights.iter().enumerate() {
                let tf = f64::from(counts[i]);
                if tf > 0.0 {
                    score += weight * tf * 2.2 / (tf + norm);
                }
                if names.contains(&i) {
                    score += 2.0 * weight;
                }
            }
            (score, document)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    documents.extend(scored.into_iter().map(|(_, document)| document));
    true
}

fn candidates_from<'a>(
    snapshot: &Snapshot,
    passages: Passages,
    memories: impl IntoIterator<Item = &'a Value>,
) -> Result<(Vec<Value>, Value)> {
    let Passages { mut rows, report } = passages;
    let source_passages = rows.len();
    let mut stale = 0;
    let mut draft = 0;
    let mut recalled = 0;
    let mut omitted_memories = 0;
    // `snapshot.memories` is newest-updated first, so past the recall bound
    // the oldest reviewed memories are the ones left out (and counted).
    for memory in memories {
        if memory["stale"] == true {
            stale += 1;
            continue;
        }
        if memory["state"] != "reviewed" {
            draft += 1;
            continue;
        }
        if recalled >= graphicle_memory::retrieval::MAX_RECALL_MEMORIES {
            omitted_memories += 1;
            continue;
        }
        // Unreachable within bounds: passages plus recall memories fit `rank`.
        if rows.len() >= graphicle_memory::retrieval::MAX_ROWS {
            return Err(Error::bad("Too many recall candidates"));
        }
        recalled += 1;
        rows.push(json!({"identity":format!("memory:{}",s(memory,"id")),"kind":"memory","origin":format!("memory:{}",s(memory,"id")),
            "title":memory["title"],"text":memory["body"],"source_sha":digest(memory),"memory":memory}));
    }
    let mut coverage = json!({"source_documents":snapshot.documents.len(),"source_passages":source_passages,
        "passage_limit_reached":report["passage_limit_reached"],"omitted_documents":report["omitted_documents"],"omitted_documents_are":report["omitted_documents_are"],
        "omitted_reviewed_memories":omitted_memories,"stale_memories":stale,"unapproved_or_retired_memories":draft});
    // Past the per-question bounds the question chose the documents read.
    if report["selected_by_question"] == true {
        coverage["selected_by_question"] = json!(true);
    }
    // Only past the document bound, so a packet within it is unchanged.
    if report["document_limit_reached"] == true {
        coverage["document_limit_reached"] = json!(true);
        coverage["omitted_by_document_limit"] = report["omitted_by_document_limit"].clone();
    }
    Ok((rows, coverage))
}
pub fn context(snapshot: &Snapshot, args: &Value, extra_rankings: Value) -> Result<Value> {
    context_with(snapshot, args, extra_rankings, None)
}
/// `context` with the code-graph arm (G04). The arm's anchored
/// hits are fused into `rank` under the name `code_graph`; anchors no chunk
/// covers become synthetic evidence items so `pack` can still cite them.
pub fn context_with(
    snapshot: &Snapshot,
    args: &Value,
    extra_rankings: Value,
    code_graph: Option<&crate::code_graph_arm::Arm>,
) -> Result<Value> {
    context_scored(snapshot, args, extra_rankings, code_graph, None)
}

/// Candidate order after cross-encoder scoring of the fused prefix (index =
/// fused rank). `blend` counts the min-max cross-encoder score and the fused
/// rank (mapped to 1.0..0.1, as Hindsight's `reranking.py` maps RRF order)
/// equally, so a small cross-encoder cannot bury what the retrieval arms
/// agreed on; otherwise the cross-encoder alone decides. Ties keep fused order.
pub fn rerank_order(scores: &[f64], blend: bool) -> Vec<usize> {
    let n = scores.len();
    let (low, high) = scores
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| {
            (a.min(x), b.max(x))
        });
    let value = |rank: usize| {
        let score = scores[rank];
        if !blend {
            return score;
        }
        let cross = if high > low {
            (score - low) / (high - low)
        } else {
            0.5
        };
        let fused = 1.0 - 0.9 * rank as f64 / (n.max(2) - 1) as f64;
        0.5 * cross + 0.5 * fused
    };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| value(b).total_cmp(&value(a)).then(a.cmp(&b)));
    order
}

/// Candidate order fusing the cross-encoder's order with the fused order,
/// 0.7/(60 + CE rank) + 0.3/(60 + fused rank) (SmartSearch, arXiv
/// 2603.15599 §3.5; index = fused rank). Ties keep fused order.
pub fn rerank_rrf_order(scores: &[f64]) -> Vec<usize> {
    let fused: Vec<usize> = (0..scores.len()).collect();
    rrf_by_ranks(&score_ranks(scores), &fused)
}

/// Each candidate's 0-based rank by descending score, ties by index.
fn score_ranks(scores: &[f64]) -> Vec<usize> {
    let mut by_score: Vec<usize> = (0..scores.len()).collect();
    by_score.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    let mut ranks = vec![0usize; scores.len()];
    for (rank, &i) in by_score.iter().enumerate() {
        ranks[i] = rank;
    }
    ranks
}

fn rrf_by_ranks(first: &[usize], second: &[usize]) -> Vec<usize> {
    let value = |i: usize| 0.7 / (60.0 + first[i] as f64) + 0.3 / (60.0 + second[i] as f64);
    let mut order: Vec<usize> = (0..first.len()).collect();
    order.sort_by(|&a, &b| value(b).total_cmp(&value(a)).then(a.cmp(&b)));
    order
}

/// Rerank a bounded fused source prefix before packing; model text comes
/// exclusively from the authorized snapshot, never an unfiltered vector hit.
pub fn context_scored(
    snapshot: &Snapshot,
    args: &Value,
    extra_rankings: Value,
    code_graph: Option<&crate::code_graph_arm::Arm>,
    reranker: Option<&crate::model::Profile>,
) -> Result<Value> {
    context_on(
        snapshot,
        args,
        extra_rankings,
        code_graph,
        reranker,
        None,
        None,
    )
}

/// The recall switches in force for one call, echoed in every packet's
/// `mechanisms.switches` so a caller can verify them (a server's fixed
/// values arrive as the request's): `graph_retrieval`, `memory_retrieval`,
/// `cross_repo` (which follows `graph_retrieval` when left out) and
/// `notes_first` (left out: [`NOTES_FIRST_DEFAULT`]) and `identifier_route`
/// (left out: [`IDENTIFIER_ROUTE_DEFAULT`]) as booleans;
/// `doc_links` and `symbol_links` as `true`, `false` or `"counted"` (left
/// out: counted, not followed); `code_first`, `query_focus`,
/// `identifier_arm` and `more_files` as asked, else `"auto"`; and `scope`.
/// A switch outside its values ([`crate::api::switch_refusal`]) is refused.
pub fn switches(args: &Value, scope: &str) -> Result<Value> {
    for field in crate::api::RECALL_SWITCHES {
        if let Some(refused) = args
            .get(field)
            .filter(|v| !v.is_null())
            .and_then(|v| crate::api::switch_refusal(field, v))
        {
            return Err(Error::bad(refused));
        }
    }
    let graph = args["graph_retrieval"] != false;
    let linked = |field: &str| match args.get(field) {
        Some(Value::Bool(on)) => json!(on),
        _ => json!("counted"),
    };
    // The code-question switches: as asked, else "auto".
    let auto = |field: &str| {
        args.get(field)
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(json!("auto"))
    };
    Ok(json!({
        "graph_retrieval": graph,
        "memory_retrieval": args["memory_retrieval"] != false,
        "doc_links": linked("doc_links"),
        "symbol_links": linked("symbol_links"),
        "cross_repo": args["cross_repo"].as_bool().unwrap_or(graph),
        "notes_first": args["notes_first"].as_bool().unwrap_or(NOTES_FIRST_DEFAULT),
        "identifier_route": args["identifier_route"].as_bool().unwrap_or(IDENTIFIER_ROUTE_DEFAULT),
        "code_first": auto("code_first"),
        "query_focus": auto("query_focus"),
        "identifier_arm": auto("identifier_arm"),
        "more_files": auto("more_files"),
        "scope": scope,
    }))
}

/// [`context_scored`] over passages already chunked from `snapshot`
/// ([`passages`]); `None` chunks the snapshot here. `doc_links` reads the
/// project's document links ([`crate::doc_links::lift`]): followed with
/// `doc_links: true`, else only counted; without it no link is read. The
/// same lookup reads reviewed memory's links to code symbols
/// ([`crate::symbol_links::lift`]): followed with `symbol_links: true`,
/// else only counted (`false` reads none).
pub fn context_on(
    snapshot: &Snapshot,
    args: &Value,
    mut extra_rankings: Value,
    code_graph: Option<&crate::code_graph_arm::Arm>,
    reranker: Option<&crate::model::Profile>,
    passages: Option<Passages>,
    doc_links: Option<&crate::doc_links::Lookup<'_>>,
) -> Result<Value> {
    let rescoring = reranker.is_some();
    for field in ["memory_retrieval", "graph_retrieval"] {
        if args.get(field).is_some_and(|v| !v.is_boolean()) {
            return Err(Error::bad(format!("{field} must be true or false")));
        }
    }
    let links = crate::doc_links::Options::parse(args)?;
    let symbol_links = crate::symbol_links::Mode::parse(args)?;
    // `graph_local`'s status when it did not rank (degraded past its
    // budget), passed beside the rankings; never fused.
    let local_status = extra_rankings
        .as_object_mut()
        .and_then(|m| m.shift_remove(crate::graph::LOCAL_STATUS));
    // The files that define a code name the question names when no source
    // file defines it (the identifier arm), which the code-first prior
    // leaves their weight; never fused.
    let defining: Vec<String> = extra_rankings
        .as_object_mut()
        .and_then(|m| m.shift_remove(crate::code_question::DEFINING))
        .and_then(|files| serde_json::from_value(files).ok())
        .unwrap_or_default();
    let graph_enabled = args["graph_retrieval"] != false;
    if !graph_enabled && let Some(rankings) = extra_rankings.as_object_mut() {
        // A precomputed property-graph arm must obey the same switch as the
        // static relationship expansion below. Keep independent dense ranks.
        rankings.shift_remove("graph_local");
    }
    let no_edges = json!([]);
    let code_edges;
    let edges = if !graph_enabled {
        &no_edges
    } else if !links.documents() {
        // `doc_links: false`: documents' file
        // edges do not lift code either.
        code_edges = json!(
            snapshot
                .graph
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|edge| !crate::doc_links::document_edge(edge))
                .collect::<Vec<_>>()
        );
        &code_edges
    } else {
        snapshot.graph.get("edges").unwrap_or(&no_edges)
    };
    // Borrow the visible memory catalogue. A query-specific scope must not
    // clone the complete document bodies and property graph.
    let mut memories = Vec::with_capacity(snapshot.memories.len());
    if args["memory_retrieval"] != false {
        let at = graphicle_memory::temporal::instant(&crate::now()).map_err(Error::bad)?;
        for memory in &snapshot.memories {
            if crate::structured_memory::context_allows(memory, args, at)? {
                memories.push(memory);
            }
        }
    }
    let question = string(args, "question", 16000)?;
    let (include_tests, tests_defaulted) = IncludeTests::parse(args)?;
    // A code question's query is its content, not its instructions
    // (crate::code_question::focus): what `rank`, the reranker and the
    // packer's focus read. The packet keeps the question.
    let focus = crate::code_question::focus(args, question)?;
    let query = focus.text.as_str();
    // A code question ranks source files ahead of documentation, tests and
    // vendored code (crate::code_question), in `rank` itself.
    let code_first = crate::code_question::decide(args, question, query)?;
    let max_tokens = integer(args, "max_tokens", 3000, 16000)?.max(256);
    let max_chars = integer(args, "max_chars", 12000, 64000)?.max(2000);
    let limit = integer(args, "limit", 8, 40)?.max(1);
    let rerank_depth = integer(args, "rerank_depth", 32, MAX_RERANK_DEPTH)?.max(limit.min(32));
    let debug_candidates = integer(args, "debug_candidates", 0, 400)?;
    // Files listed after the excerpts: `more_files` (0 to 40), left out
    // MORE_FILES_DEFAULT for a code question and none for another.
    let more_files = match args.get("more_files") {
        None | Some(Value::Null) => {
            if crate::code_question::is_code_question(question) {
                MORE_FILES_DEFAULT
            } else {
                0
            }
        }
        Some(_) => integer(
            args,
            "more_files",
            0,
            graphicle_memory::packing::MAX_NEXT_FILES,
        )?,
    };
    // `notes_first: true` (left out: NOTES_FIRST_DEFAULT) asks for the
    // notes-first order of a question in prose and one window per file.
    // Windows stay as ranked without it, and also with `include_tests: true`
    // (`--tools all` sends it: 0.1.0's packet) or an explicit `origin_quota`
    // (its windows per origin).
    let (notes_first, notes_first_defaulted) = crate::notes_first::switch(args)?;
    let as_ranked = !notes_first
        || include_tests == IncludeTests::Yes
        || args.get("origin_quota").is_some_and(|v| !v.is_null());
    // Dated excerpts: `source_age` when the call sets it, else `notes_first`.
    let dated = match args.get("source_age") {
        None | Some(Value::Null) => notes_first,
        Some(Value::Bool(on)) => *on,
        Some(_) => return Err(Error::bad("source_age must be true or false")),
    };
    // Past the call's limit, `rank` offers EXTRA_SOURCES more sources, so
    // the windows [`crate::notes_first::one_per_file`] drops (a second window
    // of a file, a copy of another file) are replaced from the fused order.
    let rank_limit = if rescoring {
        rerank_depth.min(graphicle_memory::retrieval::MAX_RANK_LIMIT)
    } else if as_ranked {
        limit
    } else {
        (limit + EXTRA_SOURCES).min(graphicle_memory::retrieval::MAX_RANK_LIMIT)
    };
    for (field, check) in [
        ("compact_citations", Value::is_boolean as fn(&Value) -> bool),
        ("reading_guidance", Value::is_boolean),
    ] {
        if args.get(field).is_some_and(|v| !v.is_null() && !check(v)) {
            return Err(Error::bad(format!("{field} must be true or false")));
        }
    }
    if args
        .get("pack_order")
        .is_some_and(|v| !v.is_null() && !matches!(v.as_str(), Some("rank" | "chronological")))
    {
        return Err(Error::bad("pack_order must be rank or chronological"));
    }
    if args
        .get("shown")
        .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|l| l.len() > 200))
    {
        return Err(Error::bad(
            "shown must list at most 200 source spans of id, byte_start and byte_end",
        ));
    }
    if let Some(focus) = args
        .get("pack_focus")
        .filter(|v| !v.is_null() && !v.is_boolean())
        && !focus.as_object().is_some_and(|f| {
            f.iter().all(|(key, value)| match key.as_str() {
                "stem" => value.is_boolean(),
                "context" => value.as_u64().is_some_and(|n| n <= 8),
                _ => false,
            })
        })
    {
        return Err(Error::bad(
            "pack_focus must be true, false or an object of stem (true or false) and context (0 to 8)",
        ));
    }
    if args
        .get("origin_quota")
        .is_some_and(|v| !v.is_null() && !v.is_boolean() && !v.is_object())
    {
        return Err(Error::bad(
            "origin_quota must be true, false or an object of max and min",
        ));
    }
    let passages = match passages {
        Some(passages) => passages,
        None => self::passages(snapshot)?,
    };
    let (mut rows, coverage) = candidates_from(snapshot, passages, memories.iter().copied())?;
    // `note_keys`: observer notes key the passages they quote (LongMemEval's
    // key expansion). Callers pass the reader's notes beside the rankings;
    // they are never fused as a ranking.
    let note_keys = crate::observer::KeyMode::parse(args.get("note_keys"))?;
    let reader_notes = extra_rankings
        .as_object_mut()
        .and_then(|m| m.shift_remove(crate::observer::NOTE_KEYS));
    let mut note_key_report = Value::Null;
    if let Some(mode) = note_keys {
        let notes = reader_notes
            .as_ref()
            .and_then(Value::as_array)
            .ok_or_else(|| Error::bad("note_keys needs the reader's observer notes"))?;
        note_key_report = crate::observer::key_passages(notes, &mut rows);
        note_key_report["mode"] = json!(mode.name());
        note_key_report["notes_state"] = json!(crate::observer::state_of(notes));
    }
    // Code-graph arm: fused only when the graph switch is on and an episode is
    // recorded; otherwise reported, never built inside an interactive query.
    let mut code_graph_report = json!({"status":"not_requested"});
    let mut code_graph_facts = serde_json::Map::new();
    let mut code_graph_unchunked = Vec::new();
    if let Some(arm) = code_graph {
        code_graph_report = arm.diagnostics.clone();
        if !graph_enabled {
            code_graph_report = json!({"status":"disabled"});
        } else if !arm.hits.is_empty() {
            let (ranking, facts, unchunked) =
                crate::code_graph_arm::fuse(arm, &rows, &snapshot.documents);
            code_graph_report["fused_candidates"] = json!(ranking.len());
            code_graph_report["unchunked"] = json!(unchunked.len());
            code_graph_facts = facts;
            code_graph_unchunked = unchunked;
            if !extra_rankings.is_object() {
                extra_rankings = json!({});
            }
            extra_rankings[crate::code_graph_arm::ARM] = json!(ranking);
        }
    }
    // A decision with no dependencies applies everywhere; one with symbol
    // links never does.
    let global: Vec<_> = rows
        .iter()
        .filter(|r| {
            r["kind"] == "memory"
                && ["decision", "context"].contains(&s(&r["memory"], "kind"))
                && r["memory"]["data"]["dependencies"]
                    .as_object()
                    .is_some_and(|v| v.is_empty())
                && r["memory"]["data"]["symbols"]
                    .as_array()
                    .is_none_or(|v| v.is_empty())
        })
        .map(|r| r["identity"].clone())
        .take(12)
        .collect();
    let mut rank_request = json!({"scope":snapshot.repository["id"],"question":query,"rows":[],"needs":args.get("needs").cloned().unwrap_or(json!([])),
        "limit":rank_limit,"mode":args["mode"].as_str().unwrap_or("auto"),"edges":[],"global_notes":global,"rankings":extra_rankings});
    // Plain copies: `json!` would serialize every passage row and graph edge
    // one by one.
    rank_request["rows"] = Value::Array(rows.clone());
    rank_request["edges"] = edges.clone();
    let (priors, code_first_report) = code_first.priors(&rows, &defining);
    if !priors.is_empty() {
        rank_request["origin_prior"] = Value::Object(priors);
    }
    // Retrieval tuning, validated by `rank`: weighted fusion, per-origin
    // quotas, stemmed BM25, a ranking of one speaker's turns and
    // caller-supplied query variants (rephrasings, keywords, hypothetical
    // answers). Unset options take `recipe_defaults`; the packet reports
    // which did.
    let defaults = recipe_defaults();
    let mut defaulted = serde_json::Map::new();
    for key in [
        "arm_weights",
        "rrf_k",
        "origin_quota",
        "stem",
        "speaker",
        "query_variants",
    ] {
        match args.get(key).filter(|v| !v.is_null()) {
            // `rank` reads any origin_quota as a gradient; omitting it keeps
            // the flat cap.
            Some(Value::Bool(false)) if key == "origin_quota" => {}
            Some(value) => rank_request[key] = value.clone(),
            None => {
                if let Some(value) = defaults.get(key) {
                    rank_request[key] = value.clone();
                    defaulted.insert(key.into(), value.clone());
                }
            }
        }
    }
    if let Some(crate::observer::KeyMode::Merge) = note_keys {
        rank_request["key_expansion"] = json!("merge");
    }
    // Default: with a dense arm, it counts SEMANTIC_WEIGHT (0.5) in the
    // fusion; the constant's comment has the evidence. Any explicit
    // `arm_weights`, even `{}`, replaces it.
    if args.get("arm_weights").is_none_or(Value::is_null)
        && extra_rankings.get("semantic").is_some()
    {
        rank_request["arm_weights"] = json!({"semantic": SEMANTIC_WEIGHT});
    }
    let arm_weights = rank_request
        .get("arm_weights")
        .cloned()
        .unwrap_or(json!({}));
    // A cross-encoder deeper than the ranked list reads on down the fused order.
    let fused_depth = debug_candidates.max(if rerank_depth > rank_limit && rescoring {
        rerank_depth
    } else {
        0
    });
    // A question that names no code takes its sources from the fused order
    // ordered again (crate::notes_first), so `rank` returns that order.
    let prose = !rescoring && !as_ranked && !crate::notes_first::names_code(question);
    let fused_depth = if prose {
        fused_depth.max(NOTES_FUSED)
    } else {
        fused_depth
    };
    let fused_depth = if more_files > 0 {
        fused_depth.max(MORE_FILES_FUSED)
    } else {
        fused_depth
    };
    if fused_depth > 0 {
        rank_request["debug_fused"] = json!(fused_depth);
    }
    // `rank` keeps at most this many passages per origin: two, or an
    // origin_quota's largest share.
    let origin_cap = rank_request.get("origin_quota").map_or(2, |quota| {
        let bound = |key: &str, default: u64| {
            quota
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or(default)
                .clamp(1, 10) as usize
        };
        bound("max", 3).max(bound("min", 1))
    });
    let mut ranked = native("rank", rank_request)?;
    let mut strength = ranked["evidence_strength"].clone();
    // `rank` was asked for EXTRA_SOURCES more; the packet's limit is the call's.
    if !rescoring {
        strength["limit"] = json!(limit);
    }
    let by_id: BTreeMap<&str, &Value> = rows.iter().map(|r| (s(r, "identity"), r)).collect();
    let mut reranker_usage = Value::Null;
    if let Some(reranker) = reranker {
        if reranker.protocol != "local_onnx" {
            return Err(Error::bad("Local reranking requires a local_onnx profile"));
        }
        // The ranked list first, then the fused order past it. The fused tail
        // precedes `rank`'s selection, so it skips repeated text and origins
        // already at `origin_cap` itself, as the selection does (without the
        // quota's gradient or leftover polling).
        let mut seen = BTreeSet::new();
        let mut bodies = BTreeSet::new();
        let mut per_origin: BTreeMap<&str, usize> = BTreeMap::new();
        let mut ids = Vec::new();
        let head = ranked["sources"].as_array().map(|v| (true, v));
        // Only a deep read uses the tail; `debug_candidates` alone must not
        // change what is scored.
        let tail = (rerank_depth > rank_limit)
            .then(|| ranked["fused"].as_array())
            .flatten()
            .map(|v| (false, v));
        for (in_head, value) in head
            .into_iter()
            .chain(tail)
            .flat_map(|(in_head, list)| list.iter().map(move |value| (in_head, value)))
        {
            if ids.len() >= rerank_depth {
                break;
            }
            let Some((id, row)) = value.as_str().and_then(|id| Some((id, *by_id.get(id)?))) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            let body = s(row, "text");
            let count = per_origin.entry(s(row, "origin")).or_default();
            if !in_head && (*count >= origin_cap || bodies.contains(body)) {
                continue;
            }
            bodies.insert(body);
            *count += 1;
            ids.push(id.to_string());
        }
        let texts = ids
            .iter()
            .map(|id| s(by_id[id.as_str()], "text").to_string())
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        // The local models score at most 32 passages per call; passages are
        // scored independently, so a deeper prefix is scored in chunks. The
        // model reads a passage's first `max_length` tokens.
        let mut scores = Vec::with_capacity(texts.len());
        let mut tokens = 0;
        for chunk in texts.chunks(32) {
            let (part, used) = crate::local_inference::rerank_counted(reranker, query, chunk)?;
            scores.extend(part);
            tokens += used;
        }
        let scores = &scores;
        // `blend` (default) bounds the cross-encoder: its min-max score and the
        // fused rank (mapped to 1.0..0.1 as Hindsight's reranking.py maps RRF
        // order) count equally, so a small cross-encoder cannot bury what every
        // retrieval arm agreed on. `replace` sorts by the cross-encoder alone.
        // `rrf` fuses the two orders 0.7/0.3 (SmartSearch, arXiv 2603.15599).
        let mode = args["rerank_mode"].as_str().unwrap_or("blend");
        if !["blend", "replace", "rrf"].contains(&mode) {
            return Err(Error::bad("rerank_mode must be blend, replace or rrf"));
        }
        if !texts.is_empty() {
            reranker_usage = json!({"model_key":crate::model::key(reranker),"input_tokens":tokens,"candidates":texts.len(),"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"mode":mode});
        }
        let order = match mode {
            "rrf" => rerank_rrf_order(scores),
            _ => rerank_order(scores, mode == "blend"),
        };
        ranked["sources"] = json!(
            order
                .into_iter()
                .take(limit)
                .map(|i| ids[i].clone())
                .collect::<Vec<_>>()
        );
        if let Some(memories) = ranked["memories"].as_array_mut() {
            memories.truncate(limit);
        }
    }
    // Notes first: the sources of a question in prose are the first files of
    // the fused order, one passage each; code files keep their places, and
    // the notes are ordered again among the notes' places by name, headings
    // and recency.
    let mut notes_report = json!({"status": if !notes_first {
        "off"
    } else if rescoring {
        "local_reranker"
    } else if as_ranked {
        "windows_as_ranked"
    } else {
        "code_question"
    }});
    if prose {
        let fused: Vec<&Value> = ranked["fused"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| by_id.get(id.as_str()?).copied())
            .collect();
        let today = crate::now();
        let (order, report) = crate::notes_first::rerank(
            &fused,
            &snapshot.documents,
            question,
            today.get(..10).unwrap_or(&today),
        );
        notes_report = report;
        if let Some(order) = order {
            ranked["sources"] = json!(order.into_iter().take(limit).collect::<Vec<_>>());
        }
    }
    let selected = |key: &str| {
        ranked[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| by_id.get(v.as_str()?))
            .map(|v| (*v).clone())
            .collect::<Vec<_>>()
    };
    let mut sources = selected("sources");
    // One window per file and one copy of identical files, then the call's
    // limit (`rank` offered EXTRA_SOURCES more).
    let (mut repeated, mut copies) = if as_ranked {
        (0, 0)
    } else {
        crate::notes_first::one_per_file(&mut sources, &snapshot.documents)
    };
    sources.truncate(limit);
    for source in &mut sources {
        if let Some(facts) = code_graph_facts.get(s(source, "identity")) {
            source["source"][crate::code_graph_arm::ARM] = facts.clone();
        }
    }
    let selected_memories = selected("memories");
    let linked = crate::memory_evidence::gather_with_memories(
        snapshot,
        &selected_memories,
        memories.iter().copied(),
    )?;
    // Interleave ordinary search and source-linked memory results. Both spend
    // the same context budget; a memory never becomes an original citation.
    let mut candidates = Vec::new();
    for rank in 0..sources.len().max(linked.hits.len()) {
        for group in [&sources, &linked.hits] {
            if let Some(hit) = group.get(rank) {
                candidates.push(hit.clone());
            }
        }
    }
    // Graph anchors without a chunk follow the fused results; they never
    // displace a ranked hit and share the same budget and omission report.
    // A node reached both through a static edge and the arm is one candidate:
    // dedupe by identity, then drop anchors a selected window already covers
    // (same document, overlapping bytes) or that cite an already cited
    // `stable_id`.
    let mut seen_identity = BTreeSet::new();
    candidates.retain(|c| seen_identity.insert(s(c, "identity").to_string()));
    let ranges: Vec<(String, u64, u64)> = candidates
        .iter()
        .filter_map(|c| {
            Some((
                s(&c["source"], "id").to_string(),
                c["source"]["byte_start"].as_u64()?,
                c["source"]["byte_end"].as_u64()?,
            ))
        })
        .collect();
    let mut cited_nodes: BTreeSet<String> = candidates
        .iter()
        .filter_map(|c| c["source"][crate::code_graph_arm::ARM]["stable_id"].as_str())
        .map(str::to_string)
        .collect();
    let mut deduped_anchors = 0usize;
    for anchor in code_graph_unchunked {
        let source = &anchor["source"];
        let (a, b) = (
            source["byte_start"].as_u64().unwrap_or(0),
            source["byte_end"].as_u64().unwrap_or(0),
        );
        let stable = source[crate::code_graph_arm::ARM]["stable_id"]
            .as_str()
            .map(str::to_string);
        let covered = ranges
            .iter()
            .any(|(id, x, y)| id == s(source, "id") && *x < b && a < *y)
            || stable.as_ref().is_some_and(|id| cited_nodes.contains(id))
            || !seen_identity.insert(s(&anchor, "identity").to_string());
        if covered {
            deduped_anchors += 1;
            continue;
        }
        if let Some(id) = stable {
            cited_nodes.insert(id);
        }
        candidates.push(anchor);
    }
    if code_graph_report.is_object() {
        code_graph_report["deduplicated"] = json!(deduped_anchors);
    }
    // What the selected windows' document links
    // reach, one hop, after the fused results and the code arm's anchors
    // (with `doc_links: true`; left out, the links are counted and nothing
    // is lifted). A failed read leaves the packet as it was and says so.
    let mut doc_links_report =
        json!({"status": if links.documents() { "not_requested" } else { "disabled" }});
    if let Some(lookup) = doc_links.filter(|_| links.documents()) {
        match crate::doc_links::lift(lookup, &links, snapshot, &rows, &sources, &candidates) {
            Ok(lifted) => {
                candidates.extend(lifted.candidates);
                doc_links_report = lifted.report;
            }
            Err(error) => doc_links_report = json!({"status":"error","error":error.message}),
        }
    }
    // Reviewed memory's links to code symbols, one hop, after the document
    // links (with `symbol_links: true`; left out, they are counted and
    // nothing is lifted; `false` reads none). A failed read leaves the
    // packet as it was and says so. A caller that reads no link at all (no
    // lookup) reports the step `unavailable`, never as if it had not been
    // asked for.
    let mut symbol_links_report = json!({"status": match symbol_links {
        crate::symbol_links::Mode::Off => "disabled",
        _ => "unavailable",
    }});
    let mut linked_memories = Vec::new();
    if symbol_links != crate::symbol_links::Mode::Off
        && let Some(lookup) = doc_links
    {
        match crate::symbol_links::lift(
            lookup.db(),
            lookup.repository(),
            symbol_links,
            snapshot,
            &rows,
            &sources,
            &selected_memories,
            &candidates,
        ) {
            Ok(lifted) => {
                candidates.extend(lifted.windows);
                linked_memories = lifted.memories;
                symbol_links_report = lifted.report;
            }
            Err(error) => symbol_links_report = json!({"status":"error","error":error.message}),
        }
    }
    // Test files after the other excerpts (`include_tests`): the packer
    // presents them last once the budget has gone to the windows in rank
    // order, so the bound and the budget below see 0.1.0's order.
    let mut test_files = json!({"include_tests":include_tests.value(),"defaulted":tests_defaulted});
    let demote = match include_tests {
        IncludeTests::Yes => false,
        IncludeTests::No => true,
        IncludeTests::Auto => {
            let named = question_names_tests(query);
            test_files["question_names_tests"] = json!(named);
            !named
        }
    };
    // Linked memories' windows, the code arm's anchors and lifted windows of
    // a file already quoted give way too.
    if !as_ranked {
        let (more_repeated, more_copies) =
            crate::notes_first::one_per_file(&mut candidates, &snapshot.documents);
        repeated += more_repeated;
        copies += more_copies;
        notes_report["one_per_file"] =
            json!({"repeated_windows":repeated,"identical_copies":copies});
    }
    candidates.truncate(40);
    if let Some(offered) = symbol_links_report
        .get("lifted_windows")
        .and_then(Value::as_u64)
    {
        let kept = candidates
            .iter()
            .filter(|c| c["source"].get(crate::symbol_links::ARM).is_some())
            .count() as u64;
        symbol_links_report["lifted_windows"] = json!(kept);
        symbol_links_report["over_candidate_limit"] = json!(offered.saturating_sub(kept));
    }
    // Lifted windows come last: count those the candidate bound kept.
    if let Some(offered) = doc_links_report.get("lifted").and_then(Value::as_u64) {
        let kept = candidates
            .iter()
            .filter(|c| c["source"].get(crate::doc_links::ARM).is_some())
            .count() as u64;
        doc_links_report["lifted"] = json!(kept);
        doc_links_report["over_candidate_limit"] = json!(offered.saturating_sub(kept));
    }
    let arms = arm_report(&ranked, &extra_rankings, &sources, &selected_memories);
    let mut hits = crate::evidence::source_hits(
        &snapshot.documents,
        &candidates,
        args["expand_context"] != false,
    )?;
    if demote {
        mark_test_files_last(&mut hits);
    }
    // Each excerpt's age, when dated: its file's last change in the
    // citation, and code it names whose file changed since. Reads files and
    // Git only; the order stays the ranking's.
    let source_age = match doc_links {
        Some(lookup) if dated => crate::phase::phase("context_age", || {
            crate::source_age::annotate(lookup, &mut hits)
        })
        .unwrap_or_else(|error| json!({"status":"error","error":error.message})),
        Some(_) => json!({"status":"disabled"}),
        None => json!({"status":"not_requested"}),
    };
    // Memories lifted through links follow the selected ones.
    let pack_memories: Vec<&Value> = selected_memories.iter().chain(&linked_memories).collect();
    // The packer centres excerpts on the query's words; the packet names the
    // question (below).
    let mut pack_request = json!({"repo":snapshot.repository,"question":query,"hits":hits,"memories":pack_memories,
        "max_tokens":max_tokens,"max_chars":max_chars,"reasons":ranked["reasons"],"coverage":coverage,"needs":args.get("needs").cloned().unwrap_or(json!([])),
        "semantic":{"loaded":extra_rankings.get("semantic").is_some()},"mode":ranked["mode"]});
    let focus_options = args.get("pack_focus").filter(|v| v.is_object());
    let pack_focus = match args.get("pack_focus").filter(|v| !v.is_null()) {
        Some(value) => value == true || value.is_object(),
        None => {
            let on = defaults["pack_focus"] == true;
            if on {
                defaulted.insert("pack_focus".into(), json!(true));
            }
            on
        }
    };
    if pack_focus {
        pack_request["pack_focus"] = focus_options.cloned().unwrap_or(json!(true));
    }
    if args["compact_citations"] == true {
        pack_request["compact_citations"] = json!(true);
    }
    // Mastra's navigable recall (a86dc4240), opt-in: date-ordered
    // presentation, references for excerpts the caller holds, and the
    // speaker-weighing guidance.
    for option in ["pack_order", "shown", "reading_guidance"] {
        if let Some(value) = args.get(option).filter(|v| !v.is_null()) {
            pack_request[option] = value.clone();
        }
    }
    // The project files of the fused order, each once: the packer lists
    // those its excerpts do not quote after them.
    if more_files > 0 {
        let mut seen = BTreeSet::new();
        let paths: Vec<&str> = ranked["fused"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| by_id.get(id.as_str()?))
            .filter(|row| s(&row["source"], "id").starts_with("file:"))
            .map(|row| s(&row["source"], "path"))
            .filter(|path| !path.is_empty() && seen.insert(*path))
            .take(limit + 3 * more_files)
            .collect();
        pack_request["next_files"] = json!({"paths": paths, "limit": more_files});
    }
    let mut packet = native("pack", pack_request)?;
    packet["question"] = json!(question);
    if let Some(listed) = packet.as_object_mut().and_then(|p| p.remove("next_files")) {
        packet["more_files"] = listed;
    }
    // How many delivered test-file excerpts had another excerpt after them
    // in rank order (0 under chronological presentation).
    if demote {
        let moved = packet
            .as_object_mut()
            .and_then(|p| p.remove("moved_last"))
            .unwrap_or(json!(0));
        test_files["moved"] = moved;
    }
    let mut evidence = Vec::new();
    for source in packet["sources"].as_array().into_iter().flatten() {
        let doc = snapshot
            .documents
            .iter()
            .find(|d| d["id"] == source["id"])
            .ok_or_else(|| Error::bad("Packed source unavailable"))?;
        let text = s(doc, "content");
        let a = source["byte_start"].as_u64().unwrap_or(0) as usize;
        let b = source["byte_end"].as_u64().unwrap_or(0) as usize;
        let _quote = text
            .get(a..b)
            .ok_or_else(|| Error::bad("Invalid packed source coordinates"))?;
        let mut window = crate::evidence::from_range(doc, a, b)?;
        for arm in [
            crate::code_graph_arm::ARM,
            crate::doc_links::ARM,
            crate::symbol_links::ARM,
        ] {
            if let Some(facts) = source.get(arm) {
                window[arm] = facts.clone();
            }
        }
        evidence.push(window);
    }
    code_graph_report["cited"] = json!(
        evidence
            .iter()
            .filter(|e| e.get(crate::code_graph_arm::ARM).is_some())
            .count()
    );
    packet["arms"] = arms;
    packet[crate::code_graph_arm::ARM] = code_graph_report;
    packet["memory_support"] = linked.coverage(&evidence);
    // B10: which builder produced the static relationship edges of this
    // snapshot — the projected code-graph episode or the legacy adapter.
    let graph_source = snapshot
        .graph
        .get("graph_source")
        .cloned()
        .unwrap_or(json!("legacy"));
    packet["graph_source"] = graph_source.clone();
    if let Some(error) = snapshot.graph.get("episode_error") {
        packet["graph_source_error"] = error.clone();
    }
    packet["mechanisms"] = json!({"reviewed_memory_enabled":args["memory_retrieval"] != false,
        "reviewed_memory_available":memories.iter().filter(|m|m["state"] == "reviewed" && m["stale"] != true).count(),
        "reviewed_memory_included":packet["memories"].as_array().map_or(0,Vec::len),
        "graph_retrieval_enabled":graph_enabled,"static_relationship_edges":edges.as_array().map_or(0,Vec::len),
        "graph_source":graph_source,"graph_episode":snapshot.graph.get("episode").cloned().unwrap_or(Value::Null),
        "graph_searched":extra_rankings.get("graph_local").is_some() || matches!(packet[crate::code_graph_arm::ARM]["status"].as_str(),Some("fused" | "no_match")),
        "graph_local_searched":extra_rankings.get("graph_local").is_some(),
        "code_graph_searched":matches!(packet[crate::code_graph_arm::ARM]["status"].as_str(),Some("fused" | "no_match")),
        "graph_candidate_passages":extra_rankings["graph_local"].as_array().map_or(0,Vec::len),
        "code_graph_candidate_passages":extra_rankings[crate::code_graph_arm::ARM].as_array().map_or(0,Vec::len),
        "source_documents_available":snapshot.documents.len(),"source_windows_included":evidence.len()});
    graph_mechanisms(
        &mut packet["mechanisms"],
        graph_enabled,
        ranked.get("static_edges"),
        local_status,
        extra_rankings.get("graph_local").is_some(),
        &memories,
    );
    // The document links, their switch and use.
    // Counters add to what other links put there: links pending (in links)
    // join `links_pending_projection`; documents the `link` job has not
    // read yet are counted apart, in documents.
    let mut rungs: BTreeMap<String, u64> = BTreeMap::new();
    for window in &evidence {
        if let Some(facts) = window.get(crate::doc_links::ARM) {
            *rungs.entry(s(facts, "rung").to_string()).or_default() += 1;
        }
    }
    let linked: u64 = rungs.values().sum();
    doc_links_report["cited"] = json!(linked);
    let pending = doc_links_report["pending"].as_u64().unwrap_or(0);
    let pending_documents = doc_links_report["pending_documents"].as_u64().unwrap_or(0);
    packet[crate::doc_links::ARM] = doc_links_report;
    let mechanisms = &mut packet["mechanisms"];
    mechanisms["doc_links_enabled"] = json!(links.enabled());
    mechanisms["doc_links_sources"] = json!(links.sources());
    let used = &mut mechanisms["links_used"];
    used["document"] = json!(used["document"].as_u64().unwrap_or(0) + linked);
    for (rung, count) in rungs {
        let before = used["by_rung"][&rung].as_u64().unwrap_or(0);
        used["by_rung"][rung] = json!(before + count);
    }
    let before = mechanisms["links_pending_projection"].as_u64().unwrap_or(0);
    mechanisms["links_pending_projection"] = json!(before + pending);
    mechanisms["doc_links_pending_documents"] = json!(pending_documents);
    // Memory links used: the lifted memories the packet kept and the lifted
    // code windows it cites, by the rung of the link followed.
    let mut memory_rungs: BTreeMap<String, u64> = BTreeMap::new();
    let mut memories_included = 0u64;
    for memory in packet["memories"].as_array().into_iter().flatten() {
        if memory["via"] == "link" {
            memories_included += 1;
            *memory_rungs
                .entry(s(&memory["link"], "rung").to_string())
                .or_default() += 1;
        }
    }
    let mut windows_cited = 0u64;
    for window in &evidence {
        if let Some(facts) = window.get(crate::symbol_links::ARM) {
            windows_cited += 1;
            *memory_rungs
                .entry(s(facts, "rung").to_string())
                .or_default() += 1;
        }
    }
    if symbol_links_report.get("considered").is_some() {
        symbol_links_report["memories_included"] = json!(memories_included);
        symbol_links_report["cited"] = json!(windows_cited);
    }
    packet[crate::symbol_links::ARM] = symbol_links_report;
    let mechanisms = &mut packet["mechanisms"];
    mechanisms["symbol_links_enabled"] = json!(symbol_links.enabled());
    mechanisms["switches"] = switches(args, "project")?;
    let used = &mut mechanisms["links_used"];
    used["memory"] =
        json!(used["memory"].as_u64().unwrap_or(0) + memories_included + windows_cited);
    for (rung, count) in memory_rungs {
        let before = used["by_rung"][&rung].as_u64().unwrap_or(0);
        used["by_rung"][rung] = json!(before + count);
    }
    packet["evidence"] = json!(evidence);
    packet["source_state"] = json!(snapshot.state);
    packet["evidence_strength"] = strength;
    packet["depth"] = json!({"limit":limit,"max_tokens":max_tokens});
    packet["arm_weights"] = arm_weights;
    packet["retrieval_defaults"] = json!(defaulted);
    packet["test_files"] = test_files;
    packet["code_first"] = code_first_report;
    packet["query_focus"] = focus.report;
    packet["source_age"] = source_age;
    notes_report["defaulted"] = json!(notes_first_defaulted);
    packet[crate::notes_first::FIELD] = notes_report;
    if let Some(stemmer) = ranked.get("stemmer") {
        packet["stemmer"] = stemmer.clone();
    }
    if let Some(quota) = ranked.get("origin_quota") {
        packet["origin_quota"] = quota.clone();
    }
    if let Some(speaker) = ranked.get("speaker") {
        packet["speaker"] = speaker.clone();
    }
    if !note_key_report.is_null() {
        packet["note_keys"] = note_key_report;
    }
    if let Some(variants) = ranked.get("query_variants") {
        packet["query_variants"] = variants.clone();
    }
    // Diagnostics for recall harnesses: fused candidate passages before
    // selection and packing. Not served evidence and never cited.
    if debug_candidates > 0 {
        packet["candidates_debug"] = json!(
            ranked["fused"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| by_id.get(id.as_str()?))
                .take(debug_candidates)
                .enumerate()
                .map(|(rank, row)| json!({"rank":rank + 1,"identity":row["identity"],"id":row["source"]["id"],
                    "byte_start":row["source"]["byte_start"],"byte_end":row["source"]["byte_end"]}))
                .collect::<Vec<_>>()
        );
    }
    packet["model_calls"] = json!(usize::from(!reranker_usage.is_null()));
    if !reranker_usage.is_null() {
        packet["local_reranker"] = reranker_usage;
    }
    Ok(packet)
}
/// The code graph's packet fields, next to the
/// other `mechanisms`:
/// - one switch per mechanism: `code_graph_enabled`, `static_edges_enabled`
///   and `graph_local_enabled` (today all follow `graph_retrieval`), and
///   `symbol_links_enabled`, `doc_links_enabled` and `scope`, which the
///   query fills after the links steps;
/// - the guards: `static_edges_aggregated` and `static_edges_capped` (with
///   `static_edges_bound` when either applied) and `graph_local_status`
///   (`searched`, `not_requested`, `disabled` or `degraded`, with
///   `graph_local_degraded` past its budget);
/// - the counters: `memories_withheld` (`by_file` counts reviewed memories
///   withheld for a changed or unavailable file, `by_symbol` those withheld
///   because a linked code symbol changed or is gone),
///   `links_followed_through_move` (links of served reviewed memories that
///   followed an exact move) and `links_pending_projection` (links and
///   quotes of reviewed memories waiting for a projection: their own
///   `symbol_links.pending`); 0 until the links steps add theirs:
///   `links_used` (`memory` and `document`, and `by_rung`), and `workspace`
///   (one project searched until workspace scope).
fn graph_mechanisms(
    mechanisms: &mut Value,
    graph_enabled: bool,
    static_edges: Option<&Value>,
    local_status: Option<Value>,
    local_ranked: bool,
    memories: &[&Value],
) {
    let Some(fields) = mechanisms.as_object_mut() else {
        return;
    };
    for switch in [
        "code_graph_enabled",
        "static_edges_enabled",
        "graph_local_enabled",
    ] {
        fields.insert(switch.into(), json!(graph_enabled));
    }
    fields.insert("symbol_links_enabled".into(), Value::Null);
    fields.insert("doc_links_enabled".into(), Value::Null);
    fields.insert("scope".into(), json!("project"));
    let guard = |key: &str| static_edges.is_some_and(|report| report[key] == true);
    fields.insert("static_edges_aggregated".into(), json!(guard("aggregated")));
    fields.insert("static_edges_capped".into(), json!(guard("capped")));
    if let Some(report) = static_edges {
        fields.insert("static_edges_bound".into(), report.clone());
    }
    let status = match &local_status {
        Some(status) => status["status"].clone(),
        None if local_ranked => json!("searched"),
        None if !graph_enabled => json!("disabled"),
        None => json!("not_requested"),
    };
    fields.insert("graph_local_status".into(), status);
    if let Some(mut report) = local_status {
        if let Some(report) = report.as_object_mut() {
            report.remove("status");
        }
        fields.insert("graph_local_degraded".into(), report);
    }
    let reviewed = || memories.iter().filter(|m| m["state"] == "reviewed");
    let reasons = |m: &&&Value| -> Vec<String> {
        m["stale_reasons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect()
    };
    let withheld = |prefixes: &[&str]| {
        reviewed()
            .filter(|m| m["stale"] == true)
            .filter(|m| {
                reasons(m)
                    .iter()
                    .any(|r| prefixes.iter().any(|p| r.starts_with(p)))
            })
            .count()
    };
    let by_file = withheld(&["source_changed_or_unavailable:file:"]);
    let by_symbol = withheld(&["symbol_changed:", "symbol_missing:"]);
    fields.insert(
        "memories_withheld".into(),
        json!({"by_file":by_file,"by_symbol":by_symbol}),
    );
    fields.insert(
        "links_used".into(),
        json!({"memory":0,"document":0,"by_rung":{}}),
    );
    let followed: u64 = reviewed()
        .filter(|m| m["stale"] != true)
        .filter_map(|m| m["symbol_links"]["followed_move"].as_u64())
        .sum();
    let pending: u64 = reviewed()
        .filter_map(|m| m["symbol_links"]["pending"].as_u64())
        .sum();
    fields.insert("links_followed_through_move".into(), json!(followed));
    fields.insert("links_pending_projection".into(), json!(pending));
    fields.insert(
        "workspace".into(),
        json!({"projects_searched":1,"cross_edges_used":0}),
    );
}

/// Per-arm contribution: candidates offered to fusion and items selected
/// that the arm ranked (`rank` records arm names as reasons per identity).
fn arm_report(
    ranked: &Value,
    extra_rankings: &Value,
    sources: &[Value],
    memories: &[Value],
) -> Value {
    let reasons = ranked["reasons"].as_object();
    let mut names: Vec<String> = vec!["lexical".into()];
    names.extend(
        extra_rankings
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys().cloned()),
    );
    let mut report = serde_json::Map::new();
    for name in names {
        let ranked_by = |identity: &str| {
            reasons
                .and_then(|r| r.get(identity))
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|v| v == &json!(name)))
        };
        let candidates = if name == "lexical" {
            reasons.map_or(0, |r| r.keys().filter(|id| ranked_by(id)).count())
        } else {
            extra_rankings[&name].as_array().map_or(0, Vec::len)
        };
        let selected = sources
            .iter()
            .chain(memories)
            .filter(|item| ranked_by(s(item, "identity")))
            .count();
        report.insert(name, json!({"candidates":candidates,"selected":selected}));
    }
    Value::Object(report)
}
pub fn semantic_ranking(
    rows: &[Value],
    vectors: &BTreeMap<String, Vec<f64>>,
    query: &[f64],
) -> Result<Value> {
    let valid: BTreeSet<_> = rows.iter().map(|r| s(r, "identity")).collect();
    let mut pairs = Vec::new();
    let norm = query.iter().map(|v| v * v).sum::<f64>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err(Error::bad("Query embedding is empty"));
    }
    for (id, v) in vectors {
        if valid.contains(id.as_str()) && v.len() == query.len() {
            let divisor = v.iter().map(|x| x * x).sum::<f64>().sqrt() * norm;
            if divisor > 0.0 {
                pairs.push((
                    id,
                    v.iter().zip(query).map(|(a, b)| a * b).sum::<f64>() / divisor,
                ));
            }
        }
    }
    pairs.sort_by(|(a, x), (b, y)| y.total_cmp(x).then(a.cmp(b)));
    Ok(json!({"semantic":pairs.into_iter().take(200).map(|(id,_)|id).collect::<Vec<_>>()}))
}
impl Store {
    pub fn project_context(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.project_context_local(workspace, token, args, None, None)
    }
    pub fn project_context_local(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        embedding: Option<&crate::model::Profile>,
        reranker: Option<&crate::model::Profile>,
    ) -> Result<Value> {
        crate::archived::check(crate::archived::Surface::Context, args)?;
        for profile in [embedding, reranker].into_iter().flatten() {
            if profile.protocol != "local_onnx" {
                return Err(Error::bad("Local retrieval requires local_onnx profiles"));
            }
        }
        let scope = crate::workspace::Scope::parse(args)?;
        // A question over the bound is shortened, keeping its content and
        // the names it gives, instead of refused (crate::code_question).
        let shortened = args
            .get("question")
            .and_then(Value::as_str)
            .and_then(crate::code_question::shorten);
        let owned;
        let args = match &shortened {
            Some((question, _)) => {
                let mut copy = args.clone();
                copy["question"] = json!(question);
                owned = copy;
                &owned
            }
            None => args,
        };
        let report = shortened.map(|(_, report)| report);
        // Retrieval runs on a read-only snapshot and records nothing about the
        // request, so a query never takes the write lock.
        let mut packet = self.authorized_read(workspace, token, "read", |db, p| match scope {
            crate::workspace::Scope::Project => {
                if args.get("max_projects").is_some() {
                    return Err(Error::bad("max_projects applies only with scope workspace"));
                }
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                // An identifier, or a question that only asks where named
                // identifiers are defined or used: their definitions and
                // references from the code graph (crate::identifier_route).
                let route = match crate::identifier_route::answer(db, p, &repository, args)? {
                    crate::identifier_route::Routed::Answered(packet) => return Ok(packet),
                    crate::identifier_route::Routed::Retrieval(route) => route,
                };
                let mut packet = project_packet(db, p, &repository, args, embedding, reranker)?.1;
                let mut route = route;
                // A question that asks more about the names it gives keeps
                // them: the identifier arm ranks their files.
                if route["reason"] == "asks_more" {
                    route["names_fused"] = json!(packet["identifier_arm"]["status"] == "fused");
                }
                packet[crate::identifier_route::REPORT] = route;
                Ok(packet)
            }
            // Every project of the workspace the principal may read, each
            // read as this project is above; the identifier route answers
            // one project's questions only.
            crate::workspace::Scope::Workspace => {
                let (on, defaulted) = crate::identifier_route::switch(args)?;
                let mut packet = crate::workspace::context(db, p, args, embedding, reranker)?;
                let mut route =
                    json!({"path":"retrieval","reason":"workspace","defaulted":defaulted});
                route[crate::identifier_route::FIELD] = json!(on);
                packet[crate::identifier_route::REPORT] = route;
                Ok(packet)
            }
        })?;
        if let Some(report) = report {
            packet["question_shortened"] = report;
        }
        Ok(packet)
    }
}

/// One project's `context` packet for `p` (who may read `repository`): its
/// authorized snapshot (restricted by `include` and `metadata`), the arms
/// over it, and the packet. Workspace recall reads every project of its
/// scope through this function, so a workspace read is authorized and
/// ranked exactly as a single-project read.
pub fn project_packet(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    args: &Value,
    embedding: Option<&crate::model::Profile>,
    reranker: Option<&crate::model::Profile>,
) -> Result<(Snapshot, Value)> {
    let mut snap = crate::phase::phase("context_snapshot", || snapshot(db, repository, p))?;
    if args["memory_retrieval"] == false {
        snap.memories.clear();
    }
    let filter = Filter::parse(args)?.map(|f| restrict(&mut snap, &f));
    let graph = crate::phase::phase("context_graph", || {
        crate::graph::for_query(db, repository, p, &snap, args)
    })?;
    let question = string(args, "question", 16000)?;
    // What the arms read: a code question's content, not its instructions
    // (crate::code_question::focus).
    let focus = crate::code_question::focus(args, question)?;
    let query = focus.text.as_str();
    // One chunking of the snapshot serves `graph_local`'s seed and
    // walk, the dense arm and the query itself; past
    // the per-question bounds the question chooses what it reads.
    let shared = crate::phase::phase("context_passages", || {
        passages_for(&mut snap, query).map(Some)
    })?;
    let mut rankings = crate::phase::phase("context_rankings", || {
        crate::graph::add_rankings_on(&snap, graph.as_ref(), query, json!({}), shared.as_ref())
    })?;
    let mut dense_report = json!({"status":"disabled"});
    let mut dense_calls = 0;
    if let Some(profile) = embedding {
        let (candidates, _) = match &shared {
            Some(passages) => candidates_from(&snap, passages.clone(), snap.memories.iter())?,
            None => candidates(&snap)?,
        };
        let valid = candidates
            .iter()
            .map(|c| (s(c, "identity"), s(c, "source_sha")))
            .collect::<BTreeMap<_, _>>();
        let mut vectors = BTreeMap::new();
        let key = crate::model::key(profile);
        for row in crate::store::rows(
            db,
            "SELECT identity,source_sha,vector FROM am_vectors WHERE repository_id=? AND model_key=?",
            rusqlite::params![s(repository, "id"), key],
        )? {
            if valid.get(s(&row, "identity")) == Some(&s(&row, "source_sha")) {
                vectors.insert(
                    s(&row, "identity").to_string(),
                    serde_json::from_str::<Vec<f64>>(s(&row, "vector"))?,
                );
            }
        }
        dense_report = json!({"status":if vectors.is_empty(){"index_unavailable"}else{"ready"},"model_key":key,"vectors":vectors.len(),"authorized_candidates":candidates.len()});
        if !vectors.is_empty() {
            let started = std::time::Instant::now();
            let (query, tokens, _) =
                crate::local_inference::embed_query(profile, &[query.to_string()])?;
            if crate::model::key(profile) != key {
                return Err(Error::conflict(
                    "Local embedding manifest changed during query",
                ));
            }
            dense_calls = 1;
            dense_report["input_tokens"] = json!(tokens);
            dense_report["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
            if let Some(extra) = semantic_ranking(&candidates, &vectors, &query[0])?.as_object() {
                for (k, v) in extra {
                    rankings[k] = v.clone();
                }
            }
            // Multi-query (Onyx; RAG-Fusion, HyDE): a dense arm
            // `semantic_q{i}` per rephrasing (query role) and per
            // hypothetical answer (passage role). Keyword variants
            // stay lexical. One local call per role.
            let variants = graphicle_memory::retrieval::query_variants(
                args.get("query_variants").unwrap_or(&Value::Null),
            )
            .map_err(Error::from)?;
            let mut embedded = Vec::new();
            for (kind, passage) in [("query", false), ("passage", true)] {
                let picked: Vec<(usize, String)> = variants
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.kind == kind)
                    .map(|(i, v)| (i, v.text.clone()))
                    .collect();
                if picked.is_empty() {
                    continue;
                }
                let texts: Vec<String> = picked.iter().map(|(_, text)| text.clone()).collect();
                let (found, tokens, _) = if passage {
                    crate::local_inference::embed(profile, &texts)?
                } else {
                    crate::local_inference::embed_query(profile, &texts)?
                };
                if found.len() != picked.len() {
                    return Err(Error::bad(
                        "Local embedding returned the wrong number of vectors",
                    ));
                }
                dense_calls += 1;
                for ((i, _), vector) in picked.iter().zip(&found) {
                    rankings[format!("semantic_q{}", i + 1)] =
                        semantic_ranking(&candidates, &vectors, vector)?["semantic"].clone();
                }
                embedded.push(json!({"kind":kind,"texts":texts.len(),"input_tokens":tokens}));
            }
            if !embedded.is_empty() {
                dense_report["query_variants"] = json!(embedded);
                dense_report["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
    // A degraded graph arm never fails an interactive query. With
    // `doc_links: false` it leaves document nodes out, and so does a code
    // question that ranks source files first (crate::code_question).
    let links = crate::doc_links::Options::parse(args)?;
    let code_first = crate::code_question::decide(args, question, query)?;
    let arm = if args["graph_retrieval"] == false {
        crate::code_graph_arm::Arm::skipped("disabled")
    } else {
        crate::phase::phase("context_arm", || {
            crate::code_graph_arm::arm_with(
                db,
                s(repository, "id"),
                &snap,
                query,
                links.documents() && !code_first.applied,
            )
        })
        .unwrap_or_else(|error| {
            let mut arm = crate::code_graph_arm::Arm::skipped("error");
            arm.diagnostics["error"] = json!(error.message);
            arm
        })
    };
    // The identifiers and paths a code question names, found in the
    // passages and at their definitions, as one more arm
    // (crate::identifier_arm).
    let identifier_report = match &shared {
        Some(passages) => {
            let found = crate::phase::phase("context_identifiers", || {
                crate::identifier_arm::arm(db, s(repository, "id"), passages, args, question, query)
            })?;
            if !rankings.is_object() {
                rankings = json!({});
            }
            if let Some(ranking) = found.ranking {
                rankings[crate::identifier_arm::ARM] = json!(ranking);
            }
            // Beside the rankings, never fused: files the code-first prior
            // leaves their weight.
            if !found.defining.is_empty() {
                rankings[crate::code_question::DEFINING] = json!(found.defining);
            }
            found.report
        }
        None => json!({"status":"no_passages"}),
    };
    let lookup = crate::doc_links::Lookup::new(db, s(repository, "id"))
        .with_folder(crate::folders::current_folder(db, s(repository, "id"))?);
    if crate::observer::KeyMode::parse(args.get("note_keys"))?.is_some() {
        rankings[crate::observer::NOTE_KEYS] = json!(crate::observer::notes_for(
            db,
            s(repository, "id"),
            &snap.documents
        )?);
    }
    let mut packet = crate::phase::phase("context_assemble", || {
        context_on(
            &snap,
            args,
            rankings,
            Some(&arm),
            reranker,
            shared,
            Some(&lookup),
        )
    })?;
    packet["local_dense"] = dense_report;
    packet["identifier_arm"] = identifier_report;
    if let Some(filter) = filter {
        packet["filter"] = filter;
    }
    packet["model_calls"] = json!(packet["model_calls"].as_u64().unwrap_or(0) + dense_calls);
    packet["freshness"] = json!(crate::invalidation::repository_freshness(
        db,
        &p.workspace,
        s(repository, "id")
    )?);
    Ok((snap, packet))
}

impl Store {
    pub fn project_status(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace,token,"read",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let snap=snapshot(db,&repository,p)?;
            let observed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM am_connectors WHERE repository_id=? AND state='active' AND json_type(config,'$.observe')='object')",[s(&repository,"id")],|r|r.get(0))?;
            Ok(json!({"repository":repository,"files":snap.documents.iter().filter(|d|d["source_kind"]=="repository_file").count(),
                "evidence_sources":snap.documents.len(),"memories":snap.memories.len(),"stale_memories":snap.memories.iter().filter(|m|m["stale"]==true).count(),
                "draft_memories":snap.memories.iter().filter(|m|m["state"]=="draft").count(),"source_state":snap.state,"native_runtime":{"enabled":true,"language":"rust","service_version":env!("CARGO_PKG_VERSION"),"core_version":graphicle_memory::VERSION},
                "structural_revision":graphicle_memory::structure::REVISION,"graph":snap.graph,"automatic_model_calls":observed,
                "write_lock":crate::store::lock_report(),
                "freshness":crate::invalidation::repository_freshness(db,&p.workspace,s(&repository,"id"))?}))
        })
    }
    pub fn read_project_source(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let docs = memory::documents(db, &repository, p)?;
            let identifier = args["source_id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| args["path"].as_str().map(|v| format!("file:{v}")))
                .ok_or_else(|| Error::bad("Select a source ID or file path"))?;
            if let Some(snapshot)=args["snapshot"].as_str().filter(|v|*v!=s(&repository,"snapshot")) {
                let path=identifier.strip_prefix("file:").ok_or_else(||Error::bad("Repository snapshots only apply to repository files"))?;
                let saved=one(db,"SELECT manifest,created_at FROM repo_snapshots WHERE repository_id=? AND sha256=?",params![s(&repository,"id"),snapshot])?.ok_or_else(||Error::bad("Snapshot unavailable"))?;
                let manifest:Value=serde_json::from_str(s(&saved,"manifest"))?;
                let sha=manifest[path]["sha256"].as_str().ok_or_else(||Error::bad("File was absent from this snapshot"))?;
                let original=one(db,"SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",params![s(&repository,"id"),sha])?.ok_or_else(||Error::bad("Historical source was removed"))?;
                if args["sha256"].as_str().is_some_and(|expected|expected!=sha) {return Err(Error::conflict("Hash does not match the requested snapshot"));}
                if graphicle_memory::text::sha256(s(&original,"content").as_bytes())!=sha {return Err(Error::bad("Historical source integrity check failed"));}
                let doc=json!({"id":identifier,"path":path,"title":path,"sha256":sha,"version":sha,"snapshot":snapshot,"source_kind":"repository_file","content":original["content"],"recorded_at":null,"observed_at":saved["created_at"],"authority":"historical_original_source"});
                return memory::read_source(&doc,args);
            }
            let source = docs
                .iter()
                .find(|d| s(d, "id") == identifier)
                .ok_or_else(|| Error::bad("Source unavailable"))?;
            if args
                .get("sha256")
                .is_some_and(|sha| !sha.is_null() && *sha != source["sha256"])
            {
                return Err(Error::conflict(
                    "Source changed; retrieve its current reference",
                ));
            }
            memory::read_source(source, args)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{rerank_order, rerank_rrf_order};

    #[test]
    fn a_question_names_tests_by_a_test_file_or_identifier() {
        use super::question_names_tests as names;
        for yes in [
            "what does test_parse_header check",
            "is TestHTTPParser flaky",
            "see tests/parser.py",
            "django/test/utils.py breaks the runner",
            "`testing/python/approx.py` fails",
            "the fixture in conftest.py",
            "parser_test.py and parser_tests.py differ",
            "run test_parser.py",
        ] {
            assert!(names(yes), "{yes}");
        }
        for no in [
            // The words alone, and names that only contain them.
            "which tests cover parse_header",
            "Where is the test for the parser?",
            "Testing the cache",
            "the parser_tests module",
            "pytest prints the wrong lexer",
            "unittest.skip is ignored",
            "attestation of the build",
            "the latest contest_results table",
            // Other languages' test-file names are not in the rule.
            "fix parser.spec.ts",
            "`header_test.go` fails",
            "how does parse_header split a raw header",
            "parser.py reads the header",
            "",
        ] {
            assert!(!names(no), "{no}");
        }
    }

    #[test]
    fn test_files_are_the_benchmarks_test_rule() {
        use serde_json::json;
        let window = |path: &str| json!({"source":{"id":format!("file:{path}"),"path":path}});
        // The registered benchmark's TEST rule (proto_card.py), including
        // pytest's testing/ folder and *_tests.py.
        for yes in [
            "tests/test_a.py",
            "pkg/tests/helpers.py",
            "django/test/utils.py",
            "testing/python/approx.py",
            "src/testing/fixtures.rs",
            "test_x.py",
            "pkg/test_util.c",
            "sklearn/utils/estimator_checks_test.py",
            "pkg/parser_tests.py",
            "pkg/conftest.py",
            "conftest.py",
            "tests\\test_win.py",
        ] {
            assert!(super::test_file(&window(yes)), "{yes}");
        }
        for no in [
            "a.py",
            "pkg/latest.py",
            "contest/results.py",
            "pkg/attest_x.py",
            "header_test.go",
            "parser.spec.ts",
            "__tests__/a.js",
            "spec/a_spec.rb",
            "Tests/A.swift",
            "pkg/testsuite.py",
            "pytest.ini",
        ] {
            assert!(!super::test_file(&window(no)), "{no}");
        }
        let mut hits = vec![
            window("tests/test_a.py"),
            window("a.py"),
            window("testing/python/approx.py"),
            json!({"source":{"id":"connector:c:tests/x","path":"tests/x"}}),
            window("pkg/parser_tests.py"),
        ];
        // A connector's document under tests/ is not a test file.
        assert_eq!(super::mark_test_files_last(&mut hits), 3);
        let marked: Vec<bool> = hits.iter().map(|h| h["last"] == true).collect();
        assert_eq!(marked, [true, false, true, false, true]);
    }

    #[test]
    fn include_tests_is_true_false_or_auto() {
        use super::IncludeTests;
        use serde_json::json;
        let parse = |value: serde_json::Value| IncludeTests::parse(&json!({"include_tests":value}));
        assert_eq!(
            IncludeTests::parse(&json!({})).unwrap(),
            (super::INCLUDE_TESTS_DEFAULT, true)
        );
        assert_eq!(
            parse(json!(null)).unwrap(),
            (super::INCLUDE_TESTS_DEFAULT, true)
        );
        assert_eq!(parse(json!(true)).unwrap(), (IncludeTests::Yes, false));
        assert_eq!(parse(json!(false)).unwrap(), (IncludeTests::No, false));
        assert_eq!(parse(json!("auto")).unwrap(), (IncludeTests::Auto, false));
        for bad in [json!("yes"), json!(1), json!({})] {
            assert!(parse(bad).is_err());
        }
    }

    #[test]
    fn lean_replies_carry_the_text_once() {
        use serde_json::json;
        let packet = json!({"markdown":"Graphicle project evidence — p\nBudget: …\n","sources":[{"id":"file:a.py"}],
            "evidence":[{"id":"file:a.py","text":"x"}],"unresolved_needs":["cents rounding"]});
        let reply = super::lean_reply("recall_project", false, &packet).unwrap();
        assert!(reply.get("structuredContent").is_none(), "{reply}");
        assert_eq!(reply["isError"], false);
        assert_eq!(
            reply["content"],
            json!([{"type":"text","text":"Graphicle project evidence — p\nBudget: …\nNeeds these excerpts do not cover (a lexical check): cents rounding\n"}])
        );
        // A need that would pass the cap is left out, never the Markdown.
        let full = "x".repeat(super::MCP_RECALL_CHARS as usize - 10);
        let big = json!({"markdown":full,"unresolved_needs":["cents rounding"]});
        let reply = super::lean_reply("recall_project", false, &big).unwrap();
        assert_eq!(reply["content"][0]["text"], json!(full));
        // A graph answer: the engine's text, then the citations block, which
        // no longer points into a structuredContent the reply leaves out.
        let nodes: Vec<_> = (0..25)
            .map(
                |n| json!({"label":format!("f{n}()"),"relpath":"a.py","line":n + 1,"checked":true}),
            )
            .collect();
        let answer = json!({"tool":"query_graph","text":"NODE f0()","episode":"e1","episode_lag":0,"nodes":nodes});
        let reply = super::lean_reply("query_graph", true, &answer).unwrap();
        assert!(reply.get("structuredContent").is_none(), "{reply}");
        assert_eq!(
            reply["content"][0],
            json!({"type":"text","text":"NODE f0()"})
        );
        let citations = reply["content"][1]["text"].as_str().unwrap();
        assert!(
            citations.starts_with("Citations (code-graph episode e1"),
            "{citations}"
        );
        assert!(
            citations.ends_with(
                "- 5 more cited nodes not listed (structured: true lists every citation)"
            ),
            "{citations}"
        );
        assert!(!citations.contains("structuredContent"), "{citations}");
        // Other tools and answers without the text keep the 0.1.0 reply.
        assert!(super::lean_reply("read_project_source", false, &packet).is_none());
        assert!(super::lean_reply("recall_project", false, &json!({"evidence":[]})).is_none());
        assert!(super::lean_reply("get_node", true, &json!({"nodes":[]})).is_none());
    }

    #[test]
    fn structured_is_taken_from_recall_and_graph_calls_only() {
        use serde_json::json;
        let mut args = json!({"question":"q","structured":true});
        assert!(super::take_structured("recall_project", false, &mut args).unwrap());
        assert_eq!(args, json!({"question":"q"}));
        let mut args = json!({"label":"f","structured":false});
        assert!(!super::take_structured("get_node", true, &mut args).unwrap());
        assert_eq!(args, json!({"label":"f"}));
        let mut args = json!({"structured":"yes"});
        assert!(super::take_structured("recall_project", false, &mut args).is_err());
        // Another tool's contract refuses it: it stays.
        let mut args = json!({"path":"a.py","structured":true});
        assert!(!super::take_structured("read_project_source", false, &mut args).unwrap());
        assert_eq!(args["structured"], true);
        let mut args = json!({"question":"q","max_chars":40000});
        super::cap_recall(&mut args);
        assert_eq!(args["max_chars"], super::MCP_RECALL_CHARS);
        let mut args = json!({"question":"q","max_chars":4000});
        super::cap_recall(&mut args);
        assert_eq!(args["max_chars"], 4000);
        let mut args = json!({"question":"q"});
        super::cap_recall(&mut args);
        assert!(args.get("max_chars").is_none());
        // Room for the needs line, under the default budget too.
        let line = "Needs these excerpts do not cover (a lexical check): cents rounding; refunds\n";
        let room = super::MCP_RECALL_CHARS - line.chars().count() as u64;
        let mut args = json!({"question":"q","needs":["cents rounding","refunds"]});
        super::cap_recall(&mut args);
        assert_eq!(args["max_chars"], room);
        let mut args =
            json!({"question":"q","needs":["cents rounding","refunds"],"max_chars":40000});
        super::cap_recall(&mut args);
        assert_eq!(args["max_chars"], room);
        let mut args =
            json!({"question":"q","needs":["cents rounding","refunds"],"max_chars":4000});
        super::cap_recall(&mut args);
        assert_eq!(args["max_chars"], 4000);
        let full = "x".repeat(room as usize);
        let packet = json!({"markdown":full,"unresolved_needs":["refunds"]});
        let reply = super::lean_reply("recall_project", false, &packet).unwrap();
        let text = reply["content"][0]["text"].as_str().unwrap();
        assert!(text.ends_with("(a lexical check): refunds\n"), "{text}");
        assert!(text.chars().count() as u64 <= super::MCP_RECALL_CHARS);
    }

    #[test]
    fn rank_fusion_weights_the_cross_encoder_over_its_partner() {
        // Against the fused order (index), the cross-encoder's pick leads.
        assert_eq!(rerank_rrf_order(&[1.0, 3.0, 2.0]), [1, 2, 0]);
        // At 0.7/0.3 the cross-encoder wins a two-way disagreement.
        assert_eq!(rerank_rrf_order(&[1.0, 2.0]), [1, 0]);
        // Ties keep the fused order.
        assert_eq!(rerank_rrf_order(&[1.0, 1.0]), [0, 1]);
        assert!(rerank_rrf_order(&[]).is_empty());
    }

    #[test]
    fn blended_reranking_bounds_the_cross_encoder_by_the_fused_order() {
        // The cross-encoder prefers the last fused candidate by a hair.
        let scores = [0.30, 0.20, 0.10, 0.31];
        assert_eq!(rerank_order(&scores, false), [3, 0, 1, 2]);
        // Blended, the cross-encoder's pick rises from fourth to third but
        // does not jump the fused leaders; a decisive score still moves up.
        assert_eq!(rerank_order(&scores, true), [0, 1, 3, 2]);
        assert_eq!(rerank_order(&[0.0, 0.0, 9.0], true), [2, 0, 1]);
        // Equal scores keep the fused order; nothing to rank is empty.
        assert_eq!(rerank_order(&[1.0, 1.0, 1.0], true), [0, 1, 2]);
        assert!(rerank_order(&[], true).is_empty());
    }

    /// P-CAP (EVAL_PLAN R5): a synced folder past the passage bound. Its files
    /// share one time and are listed by path, so the latest session files are
    /// the ones searched, and the packet says the earliest listed were left
    /// out rather than claiming they were the oldest.
    #[test]
    fn a_folder_past_the_passage_bound_keeps_its_latest_files() {
        use serde_json::{Value, json};
        let bound = graphicle_memory::ingest::MAX_PASSAGES;
        assert!(bound >= 40_000);
        let documents: Vec<Value> = (0..bound + 3)
            .map(|i| {
                let path = format!("session-{i:05}.txt");
                let content = format!("user: session {i}\n");
                json!({"id":format!("file:{path}"),"path":path,"title":path,
                    "source_kind":"repository_file","sha256":graphicle_memory::text::sha256(content.as_bytes()),
                    "content":content,"recorded_at":null,"observed_at":"2026-10-01T00:00:00Z"})
            })
            .collect();
        let snapshot = super::Snapshot {
            repository: json!({"id":"passage-bound"}),
            documents,
            memories: vec![],
            graph: json!({}),
            state: "fixture".into(),
        };
        let (rows, coverage) = super::candidates(&snapshot).unwrap();
        assert_eq!(rows.len(), bound);
        assert_eq!(rows[0]["origin"], "file:session-00003.txt");
        assert_eq!(
            rows[bound - 1]["origin"],
            format!("file:session-{:05}.txt", bound + 2)
        );
        assert_eq!(coverage["passage_limit_reached"], true);
        assert_eq!(coverage["omitted_documents"], 3);
        assert_eq!(coverage["omitted_documents_are"], "earliest_listed");
    }

    fn synthetic(
        documents: Vec<serde_json::Value>,
        edges: Vec<serde_json::Value>,
    ) -> super::Snapshot {
        use serde_json::json;
        super::Snapshot {
            repository: json!({"id":"stage-i-guards"}),
            documents,
            memories: vec![],
            graph: json!({"edges":edges,"graph_source":"episode","episode":"e1"}),
            state: "fixture".into(),
        }
    }

    fn file(path: &str, content: &str, at: &str) -> serde_json::Value {
        serde_json::json!({"id":format!("file:{path}"),"path":path,"title":path,
            "source_kind":"repository_file","sha256":graphicle_memory::text::sha256(content.as_bytes()),
            "content":content,"recorded_at":null,"observed_at":at})
    }

    /// A projected episode with more cross-file call edges
    /// than `rank` reads no longer fails `context`. Call sites between a few
    /// file pairs fold per pair (the lift is unchanged); 60,000 distinct file
    /// pairs are cut at the bound. The packet says which, and within the bound
    /// it reports neither.
    #[test]
    fn context_folds_or_caps_static_edges_past_the_bound() {
        use serde_json::json;
        let documents = vec![
            file(
                "src/auth.py",
                "def refresh_token():\n    return rotate()\n",
                "2026-10-01T00:00:00Z",
            ),
            file(
                "src/session.py",
                "def rotate():\n    return 1\n",
                "2026-10-01T00:00:00Z",
            ),
        ];
        let call = |line: usize| json!({"source":"src/auth.py","target":"src/session.py","line":line,"kind":"calls"});
        let args = json!({"question":"refresh token"});
        let within = super::context(
            &synthetic(documents.clone(), (1..=3).map(call).collect()),
            &args,
            json!({}),
        )
        .unwrap();
        assert_eq!(within["mechanisms"]["static_edges_aggregated"], false);
        assert_eq!(within["mechanisms"]["static_edges_capped"], false);
        assert!(within["mechanisms"].get("static_edges_bound").is_none());
        let folded = super::context(
            &synthetic(documents.clone(), (1..=60_000).map(call).collect()),
            &args,
            json!({}),
        )
        .unwrap();
        let mechanisms = &folded["mechanisms"];
        assert_eq!(mechanisms["static_relationship_edges"], 60_000);
        assert_eq!(mechanisms["static_edges_aggregated"], true);
        assert_eq!(mechanisms["static_edges_capped"], false);
        assert_eq!(mechanisms["static_edges_bound"]["distinct"], 1);
        assert_eq!(folded["evidence"], within["evidence"]);
        let distinct: Vec<_> = (0..60_000)
            .map(|i| json!({"source":format!("a/{}.py", i / 300),"target":format!("b/{}.py", i % 300),"line":1,"kind":"calls"}))
            .collect();
        let capped = super::context(&synthetic(documents, distinct), &args, json!({})).unwrap();
        assert_eq!(
            capped["mechanisms"]["static_edges_capped"], true,
            "{}",
            capped["mechanisms"]
        );
        assert_eq!(
            capped["mechanisms"]["static_edges_bound"]["used"],
            graphicle_memory::retrieval::MAX_STATIC_EDGES
        );
    }

    /// 50,001 documents no longer fail `context`. The
    /// oldest is left out unread and counted in `coverage.omitted_documents`
    /// (with the passage bound's own omissions), and `graph_local`, whose
    /// property graph would exceed its node budget, reports `degraded`
    /// instead of failing the query. The reserved graph counters read 0.
    #[test]
    fn past_the_document_bound_context_degrades_and_reports() {
        use serde_json::json;
        let bound = graphicle_memory::ingest::MAX_DOCUMENTS;
        let documents: Vec<_> = (0..=bound)
            .map(|i| {
                let day =
                    chrono::DateTime::from_timestamp(1_700_000_000 + i as i64 * 60, 0).unwrap();
                file(
                    &format!("notes/{i:05}.txt"),
                    &format!("note {i}\n"),
                    &day.to_rfc3339(),
                )
            })
            .collect();
        let snapshot = synthetic(documents, vec![]);
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let principal = crate::store::Principal {
            workspace: "w".into(),
            credential: "c".into(),
            hash: "h".into(),
            role: "owner".into(),
        };
        let args = json!({"question":"note 50000","graph_retrieval":true});
        let graph =
            crate::graph::for_query(&db, &snapshot.repository, &principal, &snapshot, &args)
                .unwrap()
                .unwrap();
        assert_eq!(graph["degraded"]["nodes"], bound + 1, "{graph}");
        let rankings =
            crate::graph::add_rankings(&snapshot, Some(&graph), "note 50000", json!({})).unwrap();
        let packet = super::context(&snapshot, &args, rankings).unwrap();
        let mechanisms = &packet["mechanisms"];
        assert_eq!(mechanisms["graph_local_status"], "degraded", "{mechanisms}");
        assert_eq!(
            mechanisms["graph_local_degraded"]["max_nodes"],
            crate::graph::MAX_NODES
        );
        assert_eq!(mechanisms["graph_local_searched"], false);
        assert!(
            packet["arms"].get(crate::graph::LOCAL_STATUS).is_none(),
            "{}",
            packet["arms"]
        );
        let coverage = &packet["coverage"];
        assert_eq!(coverage["document_limit_reached"], true, "{coverage}");
        assert_eq!(coverage["omitted_by_document_limit"], 1);
        assert_eq!(
            coverage["omitted_documents"],
            bound + 1 - graphicle_memory::ingest::MAX_PASSAGES
        );
        assert_eq!(coverage["omitted_documents_are"], "oldest");
        let (rows, _) = super::candidates(&snapshot).unwrap();
        assert!(
            rows.iter()
                .all(|row| row["origin"] != "file:notes/00000.txt")
        );
        assert_eq!(
            rows.last().unwrap()["origin"],
            format!("file:notes/{bound:05}.txt")
        );
        // Reserved until their batches: switches and counters at null or 0.
        assert_eq!(mechanisms["code_graph_enabled"], true);
        assert_eq!(mechanisms["symbol_links_enabled"], false);
        assert_eq!(mechanisms["scope"], "project");
        assert_eq!(
            mechanisms["memories_withheld"],
            json!({"by_file":0,"by_symbol":0})
        );
        assert_eq!(
            mechanisms["links_used"],
            json!({"memory":0,"document":0,"by_rung":{}})
        );
        assert_eq!(
            mechanisms["workspace"],
            json!({"projects_searched":1,"cross_edges_used":0})
        );
    }

    /// Core limits: a project larger than one question reads (50,001 files,
    /// past the document bound; 40,020 one-passage files, past the passage
    /// bound) of one synced folder (one time for every file). The question
    /// chooses what is read: the early-listed file that matches it is read
    /// and cited, where path order left it out, and the coverage says the
    /// question selected the documents. Within the bounds nothing changes.
    #[test]
    fn past_the_question_bounds_the_question_selects_the_files_read() {
        use serde_json::json;
        let at = "2026-10-14T00:00:00+00:00";
        for count in [
            graphicle_memory::ingest::MAX_DOCUMENTS + 1,
            graphicle_memory::ingest::MAX_PASSAGES + 20,
        ] {
            let mut documents: Vec<_> = (0..count)
                .map(|i| file(&format!("pkg/mod{i:05}.py"), &format!("value = {i}\n"), at))
                .collect();
            documents[7] = file(
                "pkg/mod00007.py",
                "def reconcile_ledger_balances():\n    return 'ledger'\n",
                at,
            );
            let mut snapshot = synthetic(documents, vec![]);
            let question = "Where are ledger balances reconciled?";
            let before = super::passages(&snapshot).unwrap();
            assert!(
                before
                    .rows
                    .iter()
                    .all(|row| row["origin"] != "file:pkg/mod00007.py"),
                "path order leaves the early file out ({count} files)"
            );
            drop(before);
            let selected = super::passages_for(&mut snapshot, question).unwrap();
            assert!(
                selected
                    .rows
                    .iter()
                    .any(|row| row["origin"] == "file:pkg/mod00007.py"),
                "the question's file is read ({count} files)"
            );
            let args = json!({"question":question});
            let packet = super::context_on(
                &snapshot,
                &args,
                json!({}),
                None,
                None,
                Some(selected),
                None,
            )
            .unwrap();
            assert_eq!(packet["coverage"]["selected_by_question"], true);
            assert!(
                packet["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item.to_string().contains("pkg/mod00007.py")),
                "{}",
                packet["evidence"]
            );
        }
        // Within the bounds the order and the passages are unchanged.
        let documents: Vec<_> = (0..3)
            .map(|i| file(&format!("a/{i}.py"), &format!("ledger = {i}\n"), at))
            .collect();
        let mut small = synthetic(documents.clone(), vec![]);
        let within = super::passages_for(&mut small, "ledger").unwrap();
        assert_eq!(small.documents, documents);
        assert!(within.report.get("selected_by_question").is_none());
    }

    /// Past its edge budget `graph_local` reports `degraded`,
    /// with the edge count, instead of failing the query. Here 200,001 call
    /// sites between two files give that many property-graph edges over two
    /// nodes, far under the node budget.
    #[test]
    fn past_the_edge_budget_graph_local_degrades_and_reports() {
        use serde_json::json;
        let documents = vec![
            file(
                "src/auth.py",
                "def refresh_token():\n    return rotate()\n",
                "2026-10-01T00:00:00Z",
            ),
            file(
                "src/session.py",
                "def rotate():\n    return 1\n",
                "2026-10-01T00:00:00Z",
            ),
        ];
        let over = crate::graph::MAX_EDGES + 1;
        let edges: Vec<_> = (1..=over)
            .map(|line| json!({"source":"src/auth.py","target":"src/session.py","line":line,"kind":"calls"}))
            .collect();
        let snapshot = synthetic(documents, edges);
        // `graph_local` reads reviewed claims and entity links: a store's schema.
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::store::Store::open(dir.path()).unwrap();
        let db = store.connect().unwrap();
        let principal = crate::store::Principal {
            workspace: "w".into(),
            credential: "c".into(),
            hash: "h".into(),
            role: "owner".into(),
        };
        let args = json!({"question":"refresh token","graph_retrieval":true});
        let graph =
            crate::graph::for_query(&db, &snapshot.repository, &principal, &snapshot, &args)
                .unwrap()
                .unwrap();
        assert_eq!(graph["degraded"]["nodes"], 2, "{graph}");
        assert_eq!(graph["degraded"]["edges"], over);
        let rankings =
            crate::graph::add_rankings(&snapshot, Some(&graph), "refresh token", json!({}))
                .unwrap();
        let packet = super::context(&snapshot, &args, rankings).unwrap();
        let mechanisms = &packet["mechanisms"];
        assert_eq!(mechanisms["graph_local_status"], "degraded", "{mechanisms}");
        assert_eq!(
            mechanisms["graph_local_degraded"],
            json!({"nodes":2,"edges":over,"max_nodes":crate::graph::MAX_NODES,
                "max_edges":crate::graph::MAX_EDGES})
        );
        // The same call sites past the static-edge bound fold to one pair.
        assert_eq!(mechanisms["static_edges_aggregated"], true);
        assert_eq!(mechanisms["static_edges_capped"], false);
        assert!(packet["evidence"].as_array().is_some_and(|e| !e.is_empty()));
    }
}
