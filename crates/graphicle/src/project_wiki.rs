//! `graphicle wiki`: a project wiki that a model writes and keeps, and its
//! health checks.
//!
//! - **Areas.** One article per area of the code: the communities of the
//!   project's code graph (the clustering the graph tools read), largest
//!   first, each with at least [`MIN_SYMBOLS`] code symbols. A thinner
//!   community joins the area it has the most graph edges with; one with
//!   none is counted and left out, and so are areas past `--max-articles`.
//! - **Inputs.** For each area: a code pack whose focus pins the area's most
//!   connected symbols, within a per-article token budget; the passages of
//!   the project's own repository documents that name the area's symbols
//!   ([`crate::doc_links::passages_naming`]); the reviewed, current
//!   memories linked to its symbols or files (retained answers among them);
//!   and the areas it connects to, by graph edges. Never captured sessions
//!   or connector documents.
//! - **Checked.** The model returns paragraphs, each with the `path:line`
//!   citations it rests on. A citation that does not hold in the working
//!   tree is dropped, and a paragraph left with none is dropped; the run
//!   counts both.
//! - **Provenance and updates.** Each article names its model, date, commit
//!   and the digest of its inputs; `manifest.json` keeps the checked reply.
//!   `wiki update` writes again only the articles whose inputs changed,
//!   drops the articles of areas that are gone, and leaves the rest
//!   byte-identical (the sections Graphicle lists itself, such as connected
//!   areas, are rendered again without a model). An area is matched to its
//!   earlier article by the share of files they have in common, so a new
//!   clustering keeps its file names. Each article's files are recorded as
//!   its sources in the dependency index ([`crate::invalidation`]), so a
//!   sync marks it stale.
//! - **Where.** `.graphicle/wiki/written/` in the project's folder by
//!   default: under the folder the installed agent rules point to, in a
//!   subfolder that the engine's `export wiki` (which replaces the `.md`
//!   files at the top of `wiki/`) never touches. When `wiki/index.md` does
//!   not exist, a short one that points to `written/index.md` is written.
//!   `--out DIR` writes elsewhere, for a wiki kept in the repository.
//! - **Cost.** Every call goes through the spend log and the profile's caps;
//!   the run also stops before a call whose worst case would bring it over
//!   `--max-cost`. `--dry-run` prints the plan and its worst case. A run
//!   that stops keeps the finished articles: the next run picks up the
//!   rest. A profile without a price runs only on this machine.
use crate::{
    Error, Result,
    local::Session,
    long_context::{self, Request, Run, Tree},
    model,
    store::{Principal, Store, s},
};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// The rules articles are written under; part of every digest.
pub const RULES: &str = "project-wiki/1";
/// The wiki's folder under the engine's `wiki/` folder.
pub const FOLDER: &str = "written";
/// The run's record of the articles, beside them.
pub const MANIFEST: &str = "manifest.json";
/// The front matter key that marks a model-written article.
pub const MARK: &str = "graphicle_wiki";
/// Code symbols an area needs for an article of its own.
pub const MIN_SYMBOLS: usize = 4;
/// Symbols an area's prompt names, and its pack pins.
const KEY_SYMBOLS: usize = 12;
/// Lines a pinned symbol covers at most.
const PIN_LINES: usize = 60;
/// Doc passages and memories per area, and their length in characters.
const PASSAGES: usize = 8;
const PASSAGE_CHARS: usize = 800;
const MEMORIES: usize = 8;
const MEMORY_CHARS: usize = 1_500;
/// Output tokens an article may use at most.
pub const OUTPUT_TOKENS: usize = 4_000;
/// The share of files an area must have in common with an earlier article
/// to keep its name.
const MATCH_SHARE: f64 = 0.5;
/// Files an article's sources section names.
const SOURCE_FILES: usize = 40;

const WIKI_INSTRUCTIONS: &str = "You write one article of a project wiki about one area of a codebase, for engineers and coding agents who read the wiki instead of the raw files. You get a code pack: a header, an architecture summary, a manifest, and file excerpts whose code lines start with their line number and a tab (the number is not part of the file). After it comes the area: its files, its most connected symbols, the areas it connects to (by their ids), the doc passages that name its code, and the reviewed memories about it. Return one JSON object that matches the schema. title: a short name for the area. summary: one line for the wiki's index. purpose: what the area does and why it exists. key_parts: its key types and functions and what each one is for. connections: how it works with the connected areas, named by their ids. docs_and_memories: what the docs and memories say about the area, and where they disagree with the code. open_questions: what the code leaves unclear. Each paragraph is a few plain sentences and lists in citations the path:line or path:start-end of the lines it relies on, from the pack or the passages given. Leave out any claim you cannot cite. Do not praise the code.";

/// The schema of an article's reply.
pub fn article_schema() -> Value {
    let paragraph = json!({"type":"object","properties":{"text":{"type":"string"},
        "citations":{"type":"array","items":{"type":"string"}}},
        "required":["text","citations"],"additionalProperties":false});
    let paragraphs = json!({"type":"array","items":paragraph});
    json!({"type":"object","properties":{"title":{"type":"string"},"summary":{"type":"string"},
        "purpose":paragraphs,"key_parts":paragraphs,"connections":paragraphs,
        "docs_and_memories":paragraphs,"open_questions":{"type":"array","items":{"type":"string"}}},
        "required":["title","summary","purpose","key_parts","connections","docs_and_memories","open_questions"],
        "additionalProperties":false})
}

/// The article's sections with paragraphs, in order, with their headings.
const SECTIONS: [(&str, &str); 4] = [
    ("purpose", "What it does"),
    ("key_parts", "Key parts"),
    ("connections", "How it connects"),
    ("docs_and_memories", "Docs and memories"),
];

/// `graphicle wiki`'s actions.
#[derive(clap::Subcommand)]
pub enum Action {
    /// Write the wiki: one cited article per area of the code and an
    /// index, each by the model. Finished articles are kept, so a run that
    /// stops (at --max-cost, for example) goes on where it ended.
    Build(WriteArgs),
    /// Write again only the articles whose sources changed, and drop those
    /// of areas that are gone; the others stay byte-identical.
    Update(WriteArgs),
    /// Health checks: stale doc mentions, stale memories, articles whose
    /// sources changed, citations that no longer hold and areas with no
    /// doc, memory or article. With a model (unless --no-model), proposed
    /// corrections, for your approval: Graphicle never edits your files.
    Check(crate::wiki_check::Args),
    /// Run `wiki update` in the background after each commit (on), or
    /// stop (off). Off by default.
    Hook(HookArgs),
}

/// The options of `wiki build` and `wiki update`.
#[derive(clap::Args, Clone)]
pub struct WriteArgs {
    /// The project: an id, a key or a synced folder (default: this folder's
    /// project, else the last synced one).
    #[arg(long)]
    pub project: Option<String>,
    /// The wiki's folder [default: .graphicle/wiki/written in the project's
    /// folder].
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// The configured model profile [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// The run's cap in US dollars at the profile's price: a call whose
    /// worst case would bring the run over it is not sent.
    #[arg(long, value_name = "USD", default_value_t = 1.0)]
    pub max_cost: f64,
    /// Articles at most, the largest areas first.
    #[arg(long, value_name = "N", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=500))]
    pub max_articles: u64,
    /// Tokens of code each article's pack holds at most.
    #[arg(long, value_name = "TOKENS", default_value_t = 12_000, value_parser = clap::value_parser!(u64).range(1_000..=200_000))]
    pub budget: u64,
    /// Print the articles to write and the worst case of their cost; call
    /// no model and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Write every article again, changed or not.
    #[arg(long)]
    pub force: bool,
    /// Print nothing (the commit hook's runs).
    #[arg(long, hide = true)]
    pub quiet: bool,
    /// Print the report as JSON (the default when the output is not a
    /// terminal).
    #[arg(long)]
    pub json: bool,
}

/// The options of `wiki hook`.
#[derive(clap::Args, Clone)]
pub struct HookArgs {
    /// on or off.
    #[arg(value_parser = ["on", "off"])]
    pub state: String,
    /// The project: an id, a key or a synced folder (default: this folder's
    /// project).
    #[arg(long)]
    pub project: Option<String>,
    /// Each run's cap in US dollars.
    #[arg(long, value_name = "USD", default_value_t = 0.5)]
    pub max_cost: f64,
    /// The configured model profile [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
}

/// Run one action.
pub async fn command(store: &Store, config: &model::Config, action: Action) -> Result<()> {
    match action {
        Action::Build(args) => write(store, config, args, false).await,
        Action::Update(args) => write(store, config, args, true).await,
        Action::Check(args) => crate::wiki_check::check(store, config, args).await,
        Action::Hook(args) => hook(store, config, args),
    }
}

/// The session, project, folder and project name a wiki command runs on.
pub struct Target {
    pub session: Session,
    pub project: String,
    pub name: String,
    pub folder: Option<PathBuf>,
}

/// The project of `requested`, brought up to date with its folder first.
pub fn target(store: &Store, config: &model::Config, requested: Option<String>) -> Result<Target> {
    let (session, project, folder) = crate::cli::long_context_target(store, config, requested)?;
    let name =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            let repository = crate::store::repo(db, p, &project)?;
            Ok(s(&repository, "name").to_string())
        })?;
    Ok(Target {
        session,
        project,
        name,
        folder,
    })
}

/// The wiki's folder: `out`, else `.graphicle/wiki/written` (the engine's
/// output folder, `GRAPHICLE_OUT` when set) in the project's folder.
pub fn folder_of(target: &Target, out: Option<&Path>) -> Result<(PathBuf, bool)> {
    if let Some(out) = out {
        return Ok((out.to_path_buf(), false));
    }
    let Some(folder) = &target.folder else {
        return Err(Error::bad(
            "This project has no synced folder here: name the wiki's folder with --out",
        ));
    };
    let engine = std::env::var("GRAPHICLE_OUT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| ".graphicle".into());
    Ok((folder.join(engine).join("wiki").join(FOLDER), true))
}

/// One code symbol of an area.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub label: String,
    pub path: String,
    pub line: usize,
    pub degree: usize,
}

/// One area of the code: a community of the code graph.
#[derive(Clone, Debug)]
pub struct Area {
    /// Its hub node's label, as the graph tools name communities.
    pub hub: String,
    /// Its code symbols, the most connected first.
    pub symbols: Vec<Symbol>,
    pub files: BTreeSet<String>,
    pub stable_ids: BTreeSet<String>,
    /// Other areas (index into the list) → graph edges between them.
    pub links: BTreeMap<usize, usize>,
    /// Its article's file name, without `.md`.
    pub slug: String,
}

/// The project's areas, and those left out.
pub struct Areas {
    pub areas: Vec<Area>,
    /// Communities under [`MIN_SYMBOLS`] code symbols with no graph edge
    /// to an area (the others join the area they have the most edges with).
    pub thin: usize,
    /// Areas past the cap on articles.
    pub left_out: usize,
}

/// Whether `path` is prose rather than code.
fn prose(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".mdx", ".rst", ".txt", ".adoc", ".markdown"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// The areas of `repository`'s current code graph, largest first, at most
/// `max`.
pub fn areas(db: &Connection, p: &Principal, repository: &str, max: usize) -> Result<Areas> {
    let resident = crate::graph_tools::current(db, p, repository)?;
    let graph = &resident.context.graph;
    let nodes = graph.nodes();
    let mut found: Vec<(Vec<usize>, Area)> = Vec::new();
    let mut small: Vec<(Vec<usize>, Area)> = Vec::new();
    for (community, members) in &resident.context.communities {
        let mut symbols = Vec::new();
        let mut files = BTreeSet::new();
        let mut stable_ids = BTreeSet::new();
        for &i in members {
            let attributes = &nodes[i].attributes;
            let Some(path) = crate::code_graph_arm::relpath(attributes) else {
                continue;
            };
            if prose(&path) {
                continue;
            }
            files.insert(path.clone());
            if attributes.get("kind").and_then(Value::as_str) == Some("file") {
                continue;
            }
            if let Some(id) = attributes.get("stable_id").and_then(Value::as_str) {
                stable_ids.insert(id.to_string());
            }
            let degree =
                graph.neighbor_indices(i, false).count() + graph.neighbor_indices(i, true).count();
            symbols.push(Symbol {
                label: graphicle::serve_search::label(graph, i),
                path,
                line: graphicle_memory::graph::span::start_line(attributes).unwrap_or(1),
                degree,
            });
        }
        if symbols.is_empty() {
            continue;
        }
        let hub = resident
            .labels
            .get(community.to_string())
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| symbols[0].label.clone());
        let list = if symbols.len() < MIN_SYMBOLS {
            &mut small
        } else {
            &mut found
        };
        list.push((
            members.clone(),
            Area {
                hub,
                symbols,
                files,
                stable_ids,
                links: BTreeMap::new(),
                slug: String::new(),
            },
        ));
    }
    found.sort_by(|a, b| {
        b.1.symbols
            .len()
            .cmp(&a.1.symbols.len())
            .then_with(|| a.1.hub.cmp(&b.1.hub))
    });
    let left_out = found.len().saturating_sub(max);
    found.truncate(max);
    let mut of_node: Vec<Option<usize>> = vec![None; nodes.len()];
    for (k, (members, _)) in found.iter().enumerate() {
        for &i in members {
            of_node[i] = Some(k);
        }
    }
    // A small community joins the area it has the most graph edges with;
    // one with none is counted and left out.
    let mut thin = 0;
    for (members, area) in small {
        let mut edges: BTreeMap<usize, usize> = BTreeMap::new();
        for &i in &members {
            for j in graph
                .neighbor_indices(i, false)
                .chain(graph.neighbor_indices(i, true))
            {
                if let Some(k) = of_node[j] {
                    *edges.entry(k).or_default() += 1;
                }
            }
        }
        let Some((&k, _)) = edges
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        else {
            thin += 1;
            continue;
        };
        for &i in &members {
            of_node[i] = Some(k);
        }
        let target = &mut found[k].1;
        target.symbols.extend(area.symbols);
        target.files.extend(area.files);
        target.stable_ids.extend(area.stable_ids);
    }
    for (_, area) in &mut found {
        area.symbols.sort_by(|a, b| {
            b.degree
                .cmp(&a.degree)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
    }
    for (u, v, _) in graph.iter_edges() {
        if let (Some(a), Some(b)) = (of_node[u], of_node[v])
            && a != b
        {
            *found[a].1.links.entry(b).or_default() += 1;
            *found[b].1.links.entry(a).or_default() += 1;
        }
    }
    Ok(Areas {
        areas: found.into_iter().map(|(_, area)| area).collect(),
        thin,
        left_out,
    })
}

/// The share of files two sets have in common (Jaccard).
fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let common = a.intersection(b).count();
    let all = a.union(b).count();
    if all == 0 {
        0.0
    } else {
        common as f64 / all as f64
    }
}

/// Name each area's article: the name of the earlier article it shares the
/// most files with (at least [`MATCH_SHARE`] of them, each name once), else
/// a new one from its hub.
pub fn assign_slugs(areas: &mut [Area], manifest: &Manifest) {
    let earlier: Vec<(String, BTreeSet<String>)> = manifest
        .articles
        .iter()
        .map(|(slug, record)| {
            let files = record["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect();
            (slug.clone(), files)
        })
        .collect();
    // Earlier names stay reserved, so a new area never takes the name of
    // an article that is gone.
    let mut used: BTreeSet<String> = earlier
        .iter()
        .map(|(slug, _)| slug.to_lowercase())
        .collect();
    used.insert("index".to_string());
    let mut taken = vec![false; earlier.len()];
    let mut pending = Vec::new();
    for (k, area) in areas.iter_mut().enumerate() {
        let best = earlier
            .iter()
            .enumerate()
            .filter(|(i, _)| !taken[*i])
            .map(|(i, (_, files))| (i, overlap(&area.files, files)))
            .filter(|(_, share)| *share >= MATCH_SHARE)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((i, _)) => {
                taken[i] = true;
                area.slug = earlier[i].0.clone();
            }
            None => pending.push(k),
        }
    }
    for k in pending {
        let base = graphicle::wiki::safe_filename(&areas[k].hub, 80);
        let mut slug = base.clone();
        let mut n = 2;
        while used.contains(&slug.to_lowercase()) {
            slug = format!("{base}_{n}");
            n += 1;
        }
        used.insert(slug.to_lowercase());
        areas[k].slug = slug;
    }
}

/// What an area's article is written from.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// (path, sha256) of its files.
    pub files: Vec<(String, String)>,
    pub passages: Vec<Value>,
    pub memories: Vec<Value>,
    /// The digest of all of it under [`RULES`].
    pub digest: String,
}

/// The reviewed, current memories linked to `area`: through a symbol
/// (`stable_id`) or a file of the area.
fn memories_of(memories: &[Value], area: &Area) -> Vec<Value> {
    let linked = |m: &Value| {
        let data = &m["data"];
        let by_symbol = data["symbols"].as_array().into_iter().flatten().any(|e| {
            let id = e["stable_id"]
                .as_str()
                .or_else(|| e["identity"]["stable_id"].as_str());
            let path = e["identity"]["relpath"]
                .as_str()
                .or_else(|| e["relpath"].as_str());
            id.is_some_and(|id| area.stable_ids.contains(id))
                || path.is_some_and(|path| area.files.contains(path))
        });
        let by_file = data["dependencies"]
            .as_object()
            .is_some_and(|deps| deps.keys().any(|path| area.files.contains(path)));
        by_symbol || by_file
    };
    memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] != true && linked(m))
        .take(MEMORIES)
        .map(|m| {
            let body: String = s(m, "body").chars().take(MEMORY_CHARS).collect();
            json!({"id":m["id"],"revision":m["revision"],"kind":m["kind"],"title":m["title"],"body":body})
        })
        .collect()
}

/// The inputs of every area.
pub fn inputs(
    db: &Connection,
    p: &Principal,
    repository: &str,
    areas: &[Area],
) -> Result<Vec<Inputs>> {
    let heads = crate::store::heads(db, repository)?;
    let row = crate::store::repo(db, p, repository)?;
    let memories = crate::memory::list(db, &row, p, false)?;
    let mut out = Vec::with_capacity(areas.len());
    for area in areas {
        let files: Vec<(String, String)> = area
            .files
            .iter()
            .map(|path| (path.clone(), heads.get(path).cloned().unwrap_or_default()))
            .collect();
        let passages = crate::doc_links::passages_naming(
            db,
            repository,
            &area.stable_ids,
            PASSAGES,
            PASSAGE_CHARS,
        )?;
        let memories = memories_of(&memories, area);
        let digest = crate::digest(&json!({"rules":RULES,"files":files,
            "passages":passages.iter().map(|p| json!([p["path"],p["line"],p["text"]])).collect::<Vec<_>>(),
            "memories":memories.iter().map(|m| json!([m["id"],m["revision"]])).collect::<Vec<_>>()}));
        out.push(Inputs {
            files,
            passages,
            memories,
            digest,
        });
    }
    Ok(out)
}

/// The wiki's record: per article, its provenance, inputs and checked
/// reply.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub articles: BTreeMap<String, Value>,
}

impl Manifest {
    /// The manifest in `dir`, if there is one.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(MANIFEST);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| Error::bad(format!("{} is not a wiki manifest", path.display())))?;
        let articles = value["articles"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        Ok(Some(Self { articles }))
    }

    /// Write the manifest in `dir`, whole or not at all.
    pub fn save(&self, dir: &Path, project: &str) -> Result<()> {
        let articles: Map<String, Value> = self
            .articles
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let value = json!({"rules":RULES,"project":project,"articles":articles});
        write_atomic(
            &dir.join(MANIFEST),
            &format!("{}\n", serde_json::to_string_pretty(&value)?),
        )
    }
}

/// Write `text` to `path` through a temporary file beside it.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Write `text` to `path` unless it already holds it; whether it wrote.
fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(false);
    }
    write_atomic(path, text)?;
    Ok(true)
}

/// Whether `text` is an article this program wrote.
pub fn is_article(text: &str) -> bool {
    text.starts_with(&format!("---\n{MARK}: "))
}

/// The prompt's area part, after the pack.
fn area_text(area: &Area, all: &[Area], inputs: &Inputs) -> String {
    let mut text = format!("\n# The area: {}\n\n", area.slug);
    let files: Vec<&str> = area.files.iter().map(String::as_str).collect();
    text.push_str(&format!(
        "Files ({}): {}\n\nIts most connected symbols:\n",
        files.len(),
        files.join(", ")
    ));
    for symbol in area.symbols.iter().take(KEY_SYMBOLS) {
        text.push_str(&format!(
            "- {} ({}:{}), {} graph edges\n",
            symbol.label, symbol.path, symbol.line, symbol.degree
        ));
    }
    if !area.links.is_empty() {
        text.push_str("\nAreas it connects to, by graph edges between them:\n");
        for (other, edges) in &area.links {
            let other = &all[*other];
            let names: Vec<&str> = other
                .symbols
                .iter()
                .take(4)
                .map(|s| s.label.as_str())
                .collect();
            text.push_str(&format!(
                "- {}: {edges} edges; its symbols include {}\n",
                other.slug,
                names.join(", ")
            ));
        }
    }
    if !inputs.passages.is_empty() {
        text.push_str("\n# Doc passages that name this area's code\n");
        for passage in &inputs.passages {
            let names: Vec<String> = passage["mentions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m.as_str().map(|m| format!("`{m}`")))
                .collect();
            let flag = if passage["stale"] == true {
                " (flagged: code it names changed after it was written)"
            } else {
                ""
            };
            text.push_str(&format!(
                "\n## {}:{} names {}{flag}\n{}\n",
                s(passage, "path"),
                passage["line"],
                names.join(", "),
                s(passage, "text")
            ));
        }
    }
    if !inputs.memories.is_empty() {
        text.push_str("\n# Reviewed memories about this area\n");
        for memory in &inputs.memories {
            text.push_str(&format!(
                "\n## {} ({}, memory {})\n{}\n",
                s(memory, "title"),
                s(memory, "kind"),
                s(memory, "id"),
                s(memory, "body")
            ));
        }
    }
    text.push_str("\nWrite the article about this area as the schema asks.\n");
    text
}

/// Counts of what the check of a reply dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub paragraphs: usize,
    pub citations: usize,
}

/// The reply with every citation checked against the working tree: a
/// citation that does not hold is dropped, and so is a paragraph left
/// with none. Text is made one line per paragraph.
pub fn check_reply(reply: &Value, tree: &mut Tree) -> (Value, Dropped) {
    let mut dropped = Dropped::default();
    let line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = json!({"title":line(reply["title"].as_str().unwrap_or("")),
        "summary":line(reply["summary"].as_str().unwrap_or(""))});
    for (key, _) in SECTIONS {
        let mut kept = Vec::new();
        for paragraph in reply[key].as_array().into_iter().flatten() {
            let text = line(paragraph["text"].as_str().unwrap_or(""));
            if text.is_empty() {
                continue;
            }
            let mut held = Vec::new();
            for cited in paragraph["citations"].as_array().into_iter().flatten() {
                let found = long_context::citations(cited.as_str().unwrap_or(""));
                let Some(citation) = found.first() else {
                    dropped.citations += 1;
                    continue;
                };
                if tree
                    .problem(&citation.path, citation.line, citation.end)
                    .is_some()
                {
                    dropped.citations += 1;
                    continue;
                }
                let text = if citation.end > citation.line {
                    format!("{}:{}-{}", citation.path, citation.line, citation.end)
                } else {
                    format!("{}:{}", citation.path, citation.line)
                };
                if !held.contains(&text) {
                    held.push(text);
                }
            }
            if held.is_empty() {
                dropped.paragraphs += 1;
                continue;
            }
            kept.push(json!({"text":text,"citations":held}));
        }
        out[key] = json!(kept);
    }
    out["open_questions"] = json!(
        reply["open_questions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|q| q.as_str().map(line))
            .filter(|q| !q.is_empty())
            .collect::<Vec<_>>()
    );
    (out, dropped)
}

/// An article's text from its record and the areas it connects to now
/// (`(slug, edges)`).
pub fn render(slug: &str, record: &Value, links: &[(String, usize)]) -> String {
    let reply = &record["reply"];
    let model = s(record, "model");
    let commit = s(record, "commit");
    let short: String = commit.chars().take(12).collect();
    let mut text = format!(
        "---\n{MARK}: {RULES}\narea: {slug}\nmodel: {model}\nprofile: {}\nwritten_at: {}\ncommit: {}\nsources_sha256: {}\n---\n\n",
        s(record, "profile"),
        s(record, "written_at"),
        if commit.is_empty() { "unknown" } else { commit },
        s(record, "digest")
    );
    let title = match s(reply, "title") {
        "" => slug,
        title => title,
    };
    text.push_str(&format!("# {title}\n\n"));
    text.push_str(&format!(
        "> Written by a model ({model}) from the code, docs and reviewed memories it cites{}. Check a claim against its citations before you rely on it; `graphicle wiki check` lists articles whose sources changed.\n\n",
        if short.is_empty() {
            String::new()
        } else {
            format!(", at commit {short}")
        }
    ));
    if !s(reply, "summary").is_empty() {
        text.push_str(&format!("{}\n\n", s(reply, "summary")));
    }
    for (key, heading) in SECTIONS {
        let paragraphs = reply[key].as_array().cloned().unwrap_or_default();
        if paragraphs.is_empty() {
            continue;
        }
        text.push_str(&format!("## {heading}\n\n"));
        for paragraph in paragraphs {
            let cites: Vec<String> = paragraph["citations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str().map(|c| format!("`{c}`")))
                .collect();
            text.push_str(&format!(
                "{} ({})\n\n",
                s(&paragraph, "text"),
                cites.join(", ")
            ));
        }
    }
    let questions = reply["open_questions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !questions.is_empty() {
        text.push_str("## Open questions\n\n");
        for question in questions {
            text.push_str(&format!("- {}\n", question.as_str().unwrap_or("")));
        }
        text.push('\n');
    }
    text.push_str("## Listed by Graphicle\n\nThis section is not written by the model.\n\n");
    if !links.is_empty() {
        text.push_str("Connected areas:\n\n");
        for (other, edges) in links {
            text.push_str(&format!("- [{other}]({other}.md): {edges} graph edges\n"));
        }
        text.push('\n');
    }
    let files: Vec<String> = record["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(|f| format!("`{f}`")))
        .collect();
    let more = files.len().saturating_sub(SOURCE_FILES);
    text.push_str(&format!(
        "Files: {}{}\n",
        files
            .iter()
            .take(SOURCE_FILES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ));
    let docs: Vec<String> = record["docs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d.as_str().map(|d| format!("`{d}`")))
        .collect();
    if !docs.is_empty() {
        text.push_str(&format!("\nDocs that name its code: {}\n", docs.join(", ")));
    }
    let memories: Vec<String> = record["memories"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| format!("{} ({})", s(m, "title"), s(m, "id")))
        .collect();
    if !memories.is_empty() {
        text.push_str(&format!("\nReviewed memories: {}\n", memories.join("; ")));
    }
    text
}

/// The index: one line per article, in the areas' order.
pub fn index(project: &str, entries: &[(String, Value)], thin: usize, left_out: usize) -> String {
    let mut text = format!(
        "# {project}: project wiki\n\nWritten by a model from the project's code, docs and reviewed memories; every paragraph of an article cites file and line. `graphicle wiki update` keeps it current and `graphicle wiki check` lists what went stale.\n\n"
    );
    for (slug, record) in entries {
        let title = match s(&record["reply"], "title") {
            "" => slug.as_str(),
            title => title,
        };
        let summary = s(&record["reply"], "summary");
        text.push_str(&format!("- [{title}]({slug}.md): {summary}\n"));
    }
    if thin > 0 || left_out > 0 {
        text.push('\n');
    }
    if thin > 0 {
        text.push_str(&format!(
            "Not written: {thin} small areas (under {MIN_SYMBOLS} code symbols each, with no graph edge to an area).\n"
        ));
    }
    if left_out > 0 {
        text.push_str(&format!(
            "Not written: {left_out} more areas past the cap on articles (--max-articles).\n"
        ));
    }
    text
}

/// The note recall adds to an excerpt of a wiki article.
pub const RECALL_NOTE: &str =
    "written by a model (graphicle wiki): read the code it cites before relying on it";

/// Recall: each repository excerpt among `hits` from a wiki article (a file
/// that starts with the article's front matter, when the wiki is kept in a
/// synced folder with `--out`) gets one note saying a model wrote it, and
/// goes after the code and docs (`last`). Returns how many.
pub fn annotate(documents: &[Value], hits: &mut [Value]) -> u64 {
    let articles: BTreeSet<&str> = documents
        .iter()
        .filter(|d| is_article(s(d, "content")))
        .map(|d| s(d, "id"))
        .collect();
    if articles.is_empty() {
        return 0;
    }
    let mut marked = 0;
    for hit in hits.iter_mut() {
        if hit["source"]["source_kind"] != "repository_file"
            || !articles.contains(s(&hit["source"], "id"))
        {
            continue;
        }
        let marks = &mut hit["source"]["age_marks"];
        if !marks.is_array() {
            *marks = json!([]);
        }
        if let Some(list) = marks.as_array_mut() {
            list.insert(0, json!(RECALL_NOTE));
        }
        hit["last"] = json!(true);
        marked += 1;
    }
    marked
}

/// The text of the pointer `wiki/index.md` written when none exists.
const POINTER: &str = "# Project wiki\n\nThe articles a model wrote about this project's code are in [written/index.md](written/index.md).\n";

/// Whether a profile's endpoint is on this machine.
pub fn on_this_machine(profile: &model::Profile) -> bool {
    let url = profile.base_url.to_ascii_lowercase();
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(&url);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[") || rest.starts_with("[::1]")
}

/// The run's spend against `--max-cost`.
pub struct Budget {
    pub max: f64,
    pub spent: f64,
    pub price: Option<model::Price>,
}

impl Budget {
    /// The cap for `profile` (named `name`): refused when the profile has
    /// no price and its endpoint is not on this machine, or the cap is not a
    /// positive number.
    pub fn new(name: &str, profile: &model::Profile, max: f64) -> Result<Self> {
        if !(max.is_finite() && max > 0.0) {
            return Err(Error::bad(
                "--max-cost must be a positive number of US dollars",
            ));
        }
        if profile.price.is_none() && !on_this_machine(profile) {
            return Err(Error::bad(format!(
                "graphicle wiki needs the profile's price to keep the run under --max-cost: set models.{name}.price (a profile on this machine needs none)"
            )));
        }
        Ok(Self {
            max,
            spent: 0.0,
            price: profile.price.clone(),
        })
    }

    /// The worst case of a call of `input` tokens and `output` at most.
    pub fn worst(&self, input: u64, output: u64) -> f64 {
        self.price
            .as_ref()
            .map_or(0.0, |price| price.cost(input, 0, output))
    }

    /// Whether a call whose worst case is `worst` fits.
    pub fn fits(&self, worst: f64) -> bool {
        self.spent + worst <= self.max + 1e-12
    }

    /// Count a finished call by its usage.
    pub fn add(&mut self, done: &model::Completion) {
        if let Some(price) = &self.price {
            self.spent += price.cost(
                done.input_tokens.unwrap_or(0),
                done.cached_tokens.unwrap_or(0),
                done.output_tokens.unwrap_or(0),
            );
        }
    }
}

/// One area's place in a run.
struct Planned {
    area: usize,
    inputs: Inputs,
    /// write: the model writes it; keep: unchanged inputs.
    write: bool,
}

/// `wiki build` and `wiki update`.
async fn write(store: &Store, config: &model::Config, args: WriteArgs, update: bool) -> Result<()> {
    let target = target(store, config, args.project.clone())?;
    let (dir, default_place) = folder_of(&target, args.out.as_deref())?;
    let earlier = Manifest::load(&dir)?;
    if update && earlier.is_none() {
        return Err(Error::bad(format!(
            "No wiki in {}: graphicle wiki build writes one",
            dir.display()
        )));
    }
    let mut manifest = earlier.unwrap_or_default();
    let (found, all_inputs) = store.authorized_read(
        &target.session.workspace,
        &target.session.capability,
        "read",
        |db, p| {
            let mut found = areas(db, p, &target.project, args.max_articles as usize)?;
            assign_slugs(&mut found.areas, &manifest);
            let all = inputs(db, p, &target.project, &found.areas)?;
            Ok((found, all))
        },
    )?;
    let plan: Vec<Planned> = all_inputs
        .into_iter()
        .enumerate()
        .map(|(area, inputs)| {
            let slug = &found.areas[area].slug;
            let same = manifest
                .articles
                .get(slug)
                .is_some_and(|record| record["digest"] == json!(inputs.digest));
            Planned {
                area,
                inputs,
                write: args.force || !same,
            }
        })
        .collect();
    let human = !args.quiet && crate::terminal::human(args.json);
    let run = Run {
        store,
        session: &target.session,
        config,
        project: target.project.clone(),
        folder: target.folder.clone(),
        model: args.model.clone(),
        budget: Some(args.budget as usize),
        layout: Some("standard".into()),
        prefix_share: None,
        json: !human,
    };
    let profiled = long_context::profile(&run);
    if args.dry_run {
        return dry_run(&args, &dir, &found, &plan, profiled.ok(), human);
    }
    let (name, mut profile) = profiled?;
    profile.output_limit = profile.output_limit.min(OUTPUT_TOKENS);
    let mut budget = Budget::new(&name, &profile, args.max_cost)?;
    std::fs::create_dir_all(&dir)?;
    let commit = target
        .folder
        .as_deref()
        .and_then(crate::code_pack::git_head)
        .unwrap_or_default();
    let mut tree = Tree::new(target.folder.clone());
    let schema = article_schema();
    let mut written = Vec::new();
    let mut failed = Vec::new();
    let mut dropped = Dropped::default();
    let mut stopped: Option<String> = None;
    let mut left: Vec<String> = Vec::new();
    // Articles of areas that are gone.
    let current: BTreeSet<&str> = found.areas.iter().map(|a| a.slug.as_str()).collect();
    let removed: Vec<String> = manifest
        .articles
        .keys()
        .filter(|slug| !current.contains(slug.as_str()))
        .cloned()
        .collect();
    for slug in &removed {
        let path = dir.join(format!("{slug}.md"));
        if std::fs::read_to_string(&path).is_ok_and(|text| is_article(&text)) {
            std::fs::remove_file(&path)?;
        }
        manifest.articles.remove(slug);
    }
    if !removed.is_empty() {
        manifest.save(&dir, &target.name)?;
    }
    for planned in plan.iter().filter(|p| p.write) {
        let area = &found.areas[planned.area];
        if stopped.is_some() {
            left.push(area.slug.clone());
            continue;
        }
        let rest = area_text(area, &found.areas, &planned.inputs);
        let user_of = |pack: &Value| format!("{}{rest}", crate::code_pack::text(pack));
        let request = Request {
            question: &area.hub,
            rest: &rest,
            instructions: WIKI_INSTRUCTIONS,
            user_of: &user_of,
            schema: Some(&schema),
        };
        let mut pack_args = long_context::pack_args(
            &run,
            &profile,
            &question_of(area),
            args.budget as usize,
            None,
        );
        pack_args["focus"] = focus_of(area);
        let pack = store.project_pack(
            &target.session.workspace,
            &target.session.capability,
            &pack_args,
        )?;
        let estimate = long_context::request_estimate(&pack, WIKI_INSTRUCTIONS, &rest);
        let worst = budget.worst(
            crate::spend::worst_input(estimate, None, None),
            profile.output_limit as u64,
        );
        if !budget.fits(worst) {
            stopped = Some(format!(
                "the next article could cost up to {}, which would bring the run over --max-cost {} (spent {})",
                crate::spend::usd(worst),
                crate::spend::usd(budget.max),
                crate::spend::usd(budget.spent)
            ));
            left.push(area.slug.clone());
            continue;
        }
        if human {
            eprintln!(
                "graphicle: writing {} ({} symbols, {} files) with {name}...",
                area.slug,
                area.symbols.len(),
                area.files.len()
            );
        }
        let sent = long_context::send(
            &run,
            "wiki",
            &name,
            &profile,
            &pack,
            &request,
            None,
            None,
            &mut |_| {},
        )
        .await;
        let done = match sent {
            Ok((Ok(done), _)) => done,
            Ok((Err(error), _)) => {
                failed.push(json!({"area":area.slug,"error":error.message}));
                continue;
            }
            // A cap refused the call: nothing more is sent.
            Err(error) => {
                stopped = Some(error.message);
                left.push(area.slug.clone());
                continue;
            }
        };
        budget.add(&done);
        let reply = match long_context::json_in(&done.text) {
            Ok(reply) if reply.is_object() => reply,
            _ => {
                failed.push(
                    json!({"area":area.slug,"error":"The model's article is not a JSON object"}),
                );
                continue;
            }
        };
        let (checked, lost) = check_reply(&reply, &mut tree);
        dropped.paragraphs += lost.paragraphs;
        dropped.citations += lost.citations;
        let record = json!({"digest":planned.inputs.digest,"model":done.model,"profile":name,
            "written_at":crate::now(),"commit":commit,"files":area.files,
            "docs":planned.inputs.passages.iter().map(|p| format!("{}:{}", s(p,"path"), p["line"])).collect::<Vec<_>>(),
            "memories":planned.inputs.memories.iter().map(|m| json!({"id":m["id"],"title":m["title"]})).collect::<Vec<_>>(),
            "dropped":{"paragraphs":lost.paragraphs,"citations":lost.citations},"reply":checked});
        let text = render(&area.slug, &record, &links_of(area, &found.areas));
        write_atomic(&dir.join(format!("{}.md", area.slug)), &text)?;
        manifest.articles.insert(area.slug.clone(), record);
        manifest.save(&dir, &target.name)?;
        written.push(area.slug.clone());
    }
    // Unchanged articles: their listed sections follow the areas as they
    // are now, without a model.
    let mut rerendered = Vec::new();
    for planned in plan.iter().filter(|p| !p.write) {
        let area = &found.areas[planned.area];
        if let Some(record) = manifest.articles.get(&area.slug) {
            let text = render(&area.slug, record, &links_of(area, &found.areas));
            if write_if_changed(&dir.join(format!("{}.md", area.slug)), &text)? {
                rerendered.push(area.slug.clone());
            }
        }
    }
    let entries: Vec<(String, Value)> = found
        .areas
        .iter()
        .filter_map(|a| {
            manifest
                .articles
                .get(&a.slug)
                .map(|r| (a.slug.clone(), r.clone()))
        })
        .collect();
    let index_text = index(&target.name, &entries, found.thin, found.left_out);
    let index_written = if entries.is_empty() {
        false
    } else {
        write_if_changed(&dir.join("index.md"), &index_text)?
    };
    if default_place
        && !entries.is_empty()
        && let Some(parent) = dir.parent()
        && !parent.join("index.md").exists()
    {
        write_atomic(&parent.join("index.md"), POINTER)?;
    }
    if !written.is_empty() || !removed.is_empty() {
        let record = record_sources(store, &target, &entries, &removed);
        if let Err(error) = record
            && human
        {
            eprintln!("graphicle: could not record the articles' sources: {error}");
        }
    }
    let kept: Vec<&str> = plan
        .iter()
        .filter(|p| !p.write)
        .map(|p| found.areas[p.area].slug.as_str())
        .collect();
    let report = json!({"result":if stopped.is_some() {"stopped"} else if failed.is_empty() {"done"} else {"partial"},
        "wiki":dir,"written":written,"kept":kept,"rerendered":rerendered,"removed":removed,
        "failed":failed,"left":left,"stopped":stopped,"index_written":index_written,
        "dropped":{"paragraphs":dropped.paragraphs,"citations":dropped.citations},
        "areas":found.areas.len(),"thin_areas":found.thin,"left_out":found.left_out,
        "model":profile.model,"profile":name,"cost_usd":round(budget.spent),"max_cost_usd":budget.max,
        "priced":budget.price.is_some()});
    if args.quiet {
        return Ok(());
    }
    if !human {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print!("{}", write_text(&report));
    Ok(())
}

fn round(cost: f64) -> f64 {
    (cost * 1_000_000.0).round() / 1_000_000.0
}

/// The question an area's pack ranks code for.
fn question_of(area: &Area) -> String {
    let names: Vec<&str> = area
        .symbols
        .iter()
        .take(KEY_SYMBOLS)
        .map(|s| s.label.as_str())
        .collect();
    format!("How do {} work together?", names.join(", "))
}

/// The pack's focus: the area's most connected symbols, each from its line
/// to the next symbol of its file (at most [`PIN_LINES`] lines).
fn focus_of(area: &Area) -> Value {
    let mut starts: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for symbol in &area.symbols {
        starts.entry(&symbol.path).or_default().push(symbol.line);
    }
    for list in starts.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    let passages: Vec<Value> = area
        .symbols
        .iter()
        .take(KEY_SYMBOLS)
        .map(|symbol| {
            let next = starts[symbol.path.as_str()]
                .iter()
                .find(|&&line| line > symbol.line)
                .map(|&line| line - 1)
                .unwrap_or(symbol.line + PIN_LINES - 1);
            let end = next.max(symbol.line).min(symbol.line + PIN_LINES - 1);
            json!({"path":symbol.path,"start_line":symbol.line,"end_line":end})
        })
        .collect();
    json!(passages)
}

/// The areas `area` connects to, by slug, the most edges first.
fn links_of(area: &Area, all: &[Area]) -> Vec<(String, usize)> {
    let mut links: Vec<(String, usize)> = area
        .links
        .iter()
        .map(|(other, edges)| (all[*other].slug.clone(), *edges))
        .collect();
    links.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    links
}

/// The dependency index: each article (`summary` kind,
/// `wiki:<project>:<slug>`) depends on its files, so a sync that changes
/// one marks the article stale; removed articles are forgotten.
fn record_sources(
    store: &Store,
    target: &Target,
    entries: &[(String, Value)],
    removed: &[String],
) -> Result<()> {
    use crate::invalidation as index;
    store.authorized(
        &target.session.workspace,
        &target.session.capability,
        "collect",
        |db, p| {
            for slug in removed {
                let id = article_id(&target.project, slug);
                index::forget_dependent(db, &p.workspace, (index::SUMMARY, &id))?;
                index::clear_stale(db, &p.workspace, index::SUMMARY, &id)?;
            }
            for (slug, record) in entries {
                let id = article_id(&target.project, slug);
                index::forget_dependent(db, &p.workspace, (index::SUMMARY, &id))?;
                for file in record["files"].as_array().into_iter().flatten() {
                    let path = file.as_str().unwrap_or("");
                    let source = index::file_id(&target.project, path);
                    index::record_dependency(
                        db,
                        &p.workspace,
                        (index::SUMMARY, &id),
                        (index::FILE, &source),
                    )?;
                }
                index::clear_stale(db, &p.workspace, index::SUMMARY, &id)?;
            }
            Ok(())
        },
    )
}

/// An article's id in the dependency index.
pub fn article_id(project: &str, slug: &str) -> String {
    format!("wiki:{project}:{slug}")
}

/// `--dry-run`: the articles to write, their worst-case input and cost.
fn dry_run(
    args: &WriteArgs,
    dir: &Path,
    found: &Areas,
    plan: &[Planned],
    profile: Option<(String, model::Profile)>,
    human: bool,
) -> Result<()> {
    let output = profile
        .as_ref()
        .map_or(OUTPUT_TOKENS, |(_, p)| p.output_limit.min(OUTPUT_TOKENS)) as u64;
    let mut items = Vec::new();
    let mut total_input = 0u64;
    let mut total_cost = 0.0;
    for planned in plan {
        let area = &found.areas[planned.area];
        let rest = area_text(area, &found.areas, &planned.inputs);
        // The pack fills its budget at most; the rest is counted by the
        // estimator's rate.
        let input = args.budget
            + crate::code_pack::estimate::tokens(
                WIKI_INSTRUCTIONS.len() + rest.len() + 32,
                crate::code_pack::estimate::PACK_TEXT_BYTES_PER_TOKEN,
            ) as u64;
        let worst = profile
            .as_ref()
            .and_then(|(_, p)| p.price.as_ref())
            .map(|price| price.cost(input, 0, output));
        if planned.write {
            total_input += input;
            total_cost += worst.unwrap_or(0.0);
        }
        items.push(
            json!({"area":area.slug,"symbols":area.symbols.len(),"files":area.files.len(),
            "action":if planned.write {"write"} else {"keep"},"input_tokens_at_most":input,
            "output_tokens_at_most":output,"worst_cost_usd":worst.map(round)}),
        );
    }
    let priced = profile.as_ref().is_some_and(|(_, p)| p.price.is_some());
    let report = json!({"result":"dry_run","wiki":dir,"articles":items,
        "to_write":plan.iter().filter(|p| p.write).count(),
        "input_tokens_at_most":total_input,"worst_cost_usd":priced.then(|| round(total_cost)),
        "max_cost_usd":args.max_cost,"thin_areas":found.thin,"left_out":found.left_out,
        "profile":profile.as_ref().map(|(name, _)| name),"model":profile.as_ref().map(|(_, p)| &p.model)});
    if !human {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let mut text = format!("Wiki: {}\n\n", dir.display());
    for item in &items {
        text.push_str(&format!(
            "  {:<5} {} ({} symbols, {} files){}\n",
            s(item, "action"),
            s(item, "area"),
            item["symbols"],
            item["files"],
            if item["action"] == "write" {
                format!(
                    ": up to {} input tokens{}",
                    crate::terminal::count(item["input_tokens_at_most"].as_u64().unwrap_or(0)),
                    item["worst_cost_usd"]
                        .as_f64()
                        .map(|c| format!(", up to {}", crate::spend::usd(c)))
                        .unwrap_or_default()
                )
            } else {
                String::new()
            }
        ));
    }
    let to_write = report["to_write"].as_u64().unwrap_or(0);
    text.push_str(&format!(
        "\n{to_write} article(s) to write, up to {} input tokens and {} output tokens each",
        crate::terminal::count(total_input / to_write.max(1)),
        crate::terminal::count(output)
    ));
    match (priced, &profile) {
        (true, Some((name, _))) => text.push_str(&format!(
            "; worst case {} at the price of {name} (the run stops at --max-cost {}).\n",
            crate::spend::usd(total_cost),
            crate::spend::usd(args.max_cost)
        )),
        (false, Some((name, _))) => text.push_str(&format!("; {name} has no price configured.\n")),
        _ => text.push_str("; no answer model is configured.\n"),
    }
    print!("{text}");
    Ok(())
}

/// The readable report of a run.
fn write_text(report: &Value) -> String {
    let list = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let mut text = format!("Wiki: {}\n", s(report, "wiki"));
    let written = list("written");
    text.push_str(&format!(
        "Written: {} article(s){}\n",
        written.len(),
        if written.is_empty() {
            String::new()
        } else {
            format!(" ({})", written.join(", "))
        }
    ));
    text.push_str(&format!(
        "Unchanged: {} article(s)\n",
        report["kept"].as_array().map_or(0, Vec::len)
    ));
    let removed = list("removed");
    if !removed.is_empty() {
        text.push_str(&format!(
            "Removed: {} article(s) of areas that are gone ({})\n",
            removed.len(),
            removed.join(", ")
        ));
    }
    let dropped = &report["dropped"];
    if dropped["paragraphs"].as_u64().unwrap_or(0) + dropped["citations"].as_u64().unwrap_or(0) > 0
    {
        text.push_str(&format!(
            "Dropped: {} paragraph(s) and {} citation(s) whose citations did not hold\n",
            dropped["paragraphs"], dropped["citations"]
        ));
    }
    for failure in report["failed"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "Failed: {}: {}\n",
            s(failure, "area"),
            s(failure, "error")
        ));
    }
    if let Some(reason) = report["stopped"].as_str() {
        let left = list("left");
        text.push_str(&format!(
            "Stopped: {reason}. {} article(s) left; run graphicle wiki build again to go on.\n",
            left.len()
        ));
    }
    text.push_str(&format!(
        "Cost: {} at the configured price, of --max-cost {} ({}, {})\n",
        if report["priced"] == true {
            crate::spend::usd(report["cost_usd"].as_f64().unwrap_or(0.0))
        } else {
            "not priced".into()
        },
        crate::spend::usd(report["max_cost_usd"].as_f64().unwrap_or(0.0)),
        s(report, "profile"),
        s(report, "model")
    ));
    text
}

/// The marked block of the commit hook for `folder`.
const HOOK_START: &str = "# graphicle-wiki-start ";
const HOOK_END: &str = "# graphicle-wiki-end";

/// The hook block that runs `wiki update` for `folder` in the background.
pub fn hook_block(
    folder: &Path,
    program: &Path,
    data_dir: Option<&Path>,
    max_cost: f64,
    model: Option<&str>,
) -> Result<String> {
    let text = |path: &Path| -> Result<String> {
        let text = path.to_string_lossy();
        if text.contains(['\n', '\r', '\0']) {
            return Err(Error::bad(format!(
                "A path with a line break cannot go into a Git hook: {}",
                path.display()
            )));
        }
        Ok(text.into_owned())
    };
    let quote = crate::git_hooks::quote;
    let data = match data_dir {
        Some(directory) => format!(" --data-dir {}", quote(&text(directory)?)),
        None => String::new(),
    };
    let model = match model {
        Some(name) if !name.contains(['\n', '\r', '\0']) => format!(" --model {}", quote(name)),
        Some(_) => return Err(Error::bad("A model name cannot hold a line break")),
        None => String::new(),
    };
    let folder_text = text(folder)?;
    Ok(format!(
        "{HOOK_START}{folder_text}
# Added by graphicle wiki hook on: writes again the wiki articles whose sources changed, in the background, at most {cost} per run. graphicle wiki hook off removes it.
(
  d=$(git rev-parse --git-dir 2>/dev/null) || exit 0
  [ -d \"$d/rebase-merge\" ] || [ -d \"$d/rebase-apply\" ] && exit 0
  graphicle={program}
  [ -x \"$graphicle\" ] || graphicle=$(command -v graphicle 2>/dev/null) || exit 0
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX
  cd {folder} || exit 0
  \"$graphicle\"{data} wiki update --project {folder} --max-cost {max_cost}{model} --quiet </dev/null >/dev/null 2>&1 &
)
{HOOK_END}",
        cost = crate::spend::usd(max_cost),
        program = quote(&text(program)?),
        folder = quote(&folder_text),
    ))
}

/// `text` (a hook) with the wiki block for `folder` set to `block`, or
/// removed when `block` is `None`.
pub fn hook_text(text: Option<&str>, folder: &Path, block: Option<&str>) -> Option<String> {
    let start = format!("{HOOK_START}{}", folder.to_string_lossy());
    let mut lines: Vec<String> = text.unwrap_or("").split('\n').map(str::to_string).collect();
    let found = lines.iter().position(|l| l == &start).map(|a| {
        let b = lines[a..]
            .iter()
            .position(|l| l == HOOK_END)
            .map_or(lines.len(), |k| a + k + 1);
        (a, b)
    });
    match (found, block) {
        (Some((a, b)), Some(block)) => {
            lines.splice(a..b, block.split('\n').map(str::to_string));
            Some(lines.join("\n"))
        }
        (Some((a, b)), None) => {
            lines.drain(a..b);
            Some(lines.join("\n"))
        }
        (None, Some(block)) => match text {
            None => Some(format!("#!/bin/sh\n{block}\n")),
            Some(text) => {
                let mut out = text.to_string();
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
                out.push_str(block);
                out.push('\n');
                Some(out)
            }
        },
        (None, None) => None,
    }
}

/// `wiki hook on|off`: the post-commit hook's wiki block.
fn hook(store: &Store, config: &model::Config, args: HookArgs) -> Result<()> {
    let target = target(store, config, args.project.clone())?;
    let Some(folder) = target.folder.clone() else {
        return Err(Error::bad("This project has no synced folder here"));
    };
    let repository = crate::git_hooks::repository(&folder)
        .ok_or_else(|| Error::bad(format!("{} is not in a Git repository", folder.display())))?;
    let hooks = repository.hooks.map_err(Error::bad)?;
    let path = hooks.join("post-commit");
    let existing = std::fs::read_to_string(&path).ok();
    let on = args.state == "on";
    let block = if on {
        // The cap must hold before a hook runs it.
        if !(args.max_cost.is_finite() && args.max_cost > 0.0) {
            return Err(Error::bad(
                "--max-cost must be a positive number of US dollars",
            ));
        }
        let program = std::env::current_exe()?;
        let data: PathBuf = store.directory.as_ref().clone();
        let default = crate::local::data_dir();
        Some(hook_block(
            &folder,
            &program,
            (data != default).then_some(data.as_path()),
            args.max_cost,
            args.model.as_deref(),
        )?)
    } else {
        None
    };
    match hook_text(existing.as_deref(), &folder, block.as_deref()) {
        Some(text) => {
            std::fs::create_dir_all(&hooks)?;
            std::fs::write(&path, text)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&path)?.permissions();
                permissions.set_mode(permissions.mode() | 0o755);
                std::fs::set_permissions(&path, permissions)?;
            }
            if on {
                println!(
                    "The post-commit hook runs graphicle wiki update for {} after each commit, at most {} per run ({}).",
                    folder.display(),
                    crate::spend::usd(args.max_cost),
                    path.display()
                );
            } else {
                println!("Removed the wiki block from {}.", path.display());
            }
        }
        None => println!("No wiki block in {}.", path.display()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(hub: &str, files: &[&str]) -> Area {
        Area {
            hub: hub.into(),
            symbols: files
                .iter()
                .enumerate()
                .map(|(i, f)| Symbol {
                    label: format!("s{i}"),
                    path: f.to_string(),
                    line: 1 + i * 10,
                    degree: 1,
                })
                .collect(),
            files: files.iter().map(|f| f.to_string()).collect(),
            stable_ids: BTreeSet::new(),
            links: BTreeMap::new(),
            slug: String::new(),
        }
    }

    #[test]
    fn an_area_keeps_the_name_of_the_article_it_shares_most_files_with() {
        let mut manifest = Manifest::default();
        manifest
            .articles
            .insert("parser".into(), json!({"files":["a.py","b.py","c.py"]}));
        manifest
            .articles
            .insert("index_2".into(), json!({"files":["z.py"]}));
        let mut areas = vec![
            area("Parser", &["a.py", "b.py", "d.py"]),
            area("index", &["x.py"]),
            area("index", &["y.py"]),
        ];
        assign_slugs(&mut areas, &manifest);
        assert_eq!(areas[0].slug, "parser");
        // "index" is the catalog's; names do not repeat.
        assert_eq!(areas[1].slug, "index_3");
        assert_eq!(areas[2].slug, "index_4");
    }

    #[test]
    fn a_citation_that_does_not_hold_is_dropped_and_so_is_its_paragraph() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.py"), "one\ntwo\nthree\n").unwrap();
        let mut tree = Tree::new(Some(dir.path().to_path_buf()));
        let reply = json!({"title":" Parser ","summary":"Reads\nheaders.","purpose":[
            {"text":"Holds.","citations":["a.py:2","a.py:9","b.py:1"]},
            {"text":"Gone.","citations":["b.py:1"]},
            {"text":"No cites.","citations":[]}],
            "key_parts":[{"text":"Range.","citations":["a.py:1-3","see a.py:2"]}],
            "connections":[],"docs_and_memories":[],"open_questions":["Why?",""]});
        let (checked, dropped) = check_reply(&reply, &mut tree);
        assert_eq!(checked["title"], "Parser");
        assert_eq!(checked["summary"], "Reads headers.");
        assert_eq!(
            checked["purpose"],
            json!([{"text":"Holds.","citations":["a.py:2"]}])
        );
        assert_eq!(
            checked["key_parts"][0]["citations"],
            json!(["a.py:1-3", "a.py:2"])
        );
        assert_eq!(checked["open_questions"], json!(["Why?"]));
        assert_eq!(
            dropped,
            Dropped {
                paragraphs: 2,
                citations: 3
            }
        );
    }

    #[test]
    fn an_article_names_its_provenance_and_marks_the_listed_part() {
        let record = json!({"digest":"d1","model":"m-1","profile":"p","written_at":"2026-11-04T00:00:00Z",
            "commit":"0123456789abcdef","files":["a.py"],"docs":["docs/a.md:3"],
            "memories":[{"id":"m1","title":"Use UTC"}],
            "reply":{"title":"Parser","summary":"Reads headers.","purpose":[{"text":"Holds.","citations":["a.py:2"]}],
                "key_parts":[],"connections":[],"docs_and_memories":[],"open_questions":["Why?"]}});
        let text = render("parser", &record, &[("io".into(), 3)]);
        assert!(is_article(&text));
        assert!(text.contains("\nsources_sha256: d1\n"));
        assert!(text.contains("at commit 0123456789ab."));
        assert!(text.contains("## What it does\n\nHolds. (`a.py:2`)\n"));
        assert!(!text.contains("## Key parts"));
        assert!(text.contains("- [io](io.md): 3 graph edges"));
        assert!(text.contains("Docs that name its code: `docs/a.md:3`"));
        assert!(text.contains("Reviewed memories: Use UTC (m1)"));
        // The same record renders the same bytes.
        assert_eq!(text, render("parser", &record, &[("io".into(), 3)]));
    }

    #[test]
    fn the_hook_block_is_added_replaced_and_removed() {
        let folder = Path::new("/w/p");
        let block = hook_block(folder, Path::new("/bin/graphicle"), None, 0.5, None).unwrap();
        assert!(block.contains("wiki update --project '/w/p' --max-cost 0.5 --quiet"));
        let added = hook_text(None, folder, Some(&block)).unwrap();
        assert!(added.starts_with("#!/bin/sh\n# graphicle-wiki-start /w/p\n"));
        let other = hook_text(Some("#!/bin/sh\necho hi\n"), folder, Some(&block)).unwrap();
        assert!(other.starts_with("#!/bin/sh\necho hi\n\n# graphicle-wiki-start"));
        let changed = hook_block(folder, Path::new("/bin/graphicle"), None, 0.25, None).unwrap();
        let replaced = hook_text(Some(&other), folder, Some(&changed)).unwrap();
        assert!(replaced.contains("--max-cost 0.25"));
        assert!(!replaced.contains("--max-cost 0.5 "));
        let removed = hook_text(Some(&replaced), folder, None).unwrap();
        assert!(!removed.contains("graphicle-wiki"));
        assert!(removed.contains("echo hi"));
        assert_eq!(hook_text(Some(&removed), folder, None), None);
    }

    #[test]
    fn only_a_profile_on_this_machine_runs_without_a_price() {
        let mut profile: model::Profile = serde_json::from_value(json!({"protocol":"openai_chat",
            "base_url":"http://127.0.0.1:8080/v1","model":"local","input_limit":8000,"output_limit":1000}))
        .unwrap();
        assert!(Budget::new("local", &profile, 1.0).is_ok());
        profile.base_url = "https://api.example.com/v1".into();
        assert!(Budget::new("remote", &profile, 1.0).is_err());
        profile.price = Some(serde_json::from_value(json!({"input":1.0,"output":2.0})).unwrap());
        let budget = Budget::new("remote", &profile, 1.0).unwrap();
        assert!(budget.fits(budget.worst(100_000, 100_000)));
        assert!(!budget.fits(budget.worst(1_000_000, 0) + 0.5));
        assert!(Budget::new("remote", &profile, 0.0).is_err());
    }
}
