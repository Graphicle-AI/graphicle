//! `graphicle wiki`: a project wiki that a model writes and keeps, and its
//! health checks.
//!
//! - **Areas.** One article per area of the code: the communities of the
//!   project's code graph (the clustering the graph tools read), largest
//!   first, each with at least [`MIN_SYMBOLS`] code symbols. A thinner
//!   community joins the area it has the most graph edges with; one with
//!   none is counted and left out. An area that has an article keeps it
//!   while the area exists; new articles stop at `--max-articles`, which
//!   the wiki keeps with its `--budget` for later runs.
//! - **Inputs.** For each area: a code pack whose focus pins the area's most
//!   connected symbols, within a per-article token budget; the passages of
//!   the project's own repository documents that name the area's symbols
//!   ([`crate::doc_links::passages_naming`], never the wiki's own
//!   articles); the reviewed, current
//!   memories linked to its symbols or files (retained answers among them);
//!   and the areas it connects to, by graph edges. Never captured sessions
//!   or connector documents.
//! - **Checked.** The model returns paragraphs, each with the `path:line`
//!   citations it rests on. A citation must fall in the lines the request
//!   showed the model (the pack's excerpts and the doc passages) and hold
//!   in the working tree; one that does not is dropped, and a paragraph
//!   left with none is dropped. The run counts both.
//! - **Provenance and updates.** Each article names its model, date, commit
//!   and the digest of its inputs; `manifest.json` keeps the checked reply.
//!   `wiki update` writes again only the articles whose inputs changed and
//!   drops the articles of areas that are gone. The others keep the text
//!   the model wrote; only the sections Graphicle lists itself, such as
//!   connected areas, are rendered again without a model, so an article
//!   whose listed part did not change keeps its bytes. An area is matched to its
//!   earlier article by the share of files they have in common, so a new
//!   clustering keeps its file names. Each article's files are recorded as
//!   its sources in the dependency index ([`crate::invalidation`]), so a
//!   sync marks it stale.
//! - **Where.** `.graphicle/wiki/written/` in the project's folder by
//!   default: under the folder the installed agent rules point to, in a
//!   subfolder that the engine's `export wiki` (which replaces the `.md`
//!   files at the top of `wiki/`) never touches. When `wiki/index.md` does
//!   not exist, a short one that points to `written/index.md` is written.
//!   `--out DIR` writes elsewhere, for a wiki kept in the repository. A
//!   file there that this program did not write (its front matter says) is
//!   never replaced, and a lock file keeps two runs from writing at once.
//! - **Cost.** Every call goes through the spend log and the profile's caps;
//!   the run also stops before a call whose worst case would bring it over
//!   `--max-cost`. `--dry-run` prints the plan and its worst case. A run
//!   that stops keeps the finished articles: the next run picks up the
//!   rest. A failed call is counted at its logged cost (a reply cut at the
//!   output limit is billed), else at its worst case, and
//!   [`FAILURES_IN_A_ROW`] failures stop the run. A profile without a
//!   price runs only on this machine.
use crate::db::Connection;
use crate::{
    Error, Result,
    local::Session,
    long_context::{self, Request, Run, Tree},
    model,
    store::{Principal, Store, s},
};
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
/// How many times [`OUTPUT_TOKENS`] a profile with a reasoning effort may
/// use: its reasoning tokens count as output.
const REASONING_FACTOR: usize = 4;
/// Failed calls in a row that stop a run.
pub const FAILURES_IN_A_ROW: usize = 3;
/// The lock a run that writes holds in the wiki's folder.
const LOCK: &str = ".graphicle-wiki.lock";
/// `--max-articles` and `--budget` when neither the command nor the wiki
/// names them.
const DEFAULT_MAX_ARTICLES: u64 = 30;
const DEFAULT_BUDGET: u64 = 12_000;
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
    /// of areas that are gone; the others keep the text the model wrote.
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
    /// Articles at most, the largest areas first; an area that has an
    /// article keeps it while the area exists [default: the wiki's own,
    /// else 30].
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..=500))]
    pub max_articles: Option<u64>,
    /// Tokens of code each article's pack holds at most [default: the
    /// wiki's own, else 12000].
    #[arg(long, value_name = "TOKENS", value_parser = clap::value_parser!(u64).range(1_000..=200_000))]
    pub budget: Option<u64>,
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

/// The files of a wiki kept in the project's folder that are not prose:
/// they are never part of an area of the code.
#[derive(Clone, Debug, Default)]
pub struct Own {
    /// The wiki folder's path in the project, with a final `/` (empty at
    /// the project's top), when it is in the project's folder.
    prefix: Option<String>,
}

impl Own {
    /// The wiki in `dir` for `target`.
    pub fn of(target: &Target, dir: &Path) -> Self {
        let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let prefix = target.folder.as_ref().and_then(|folder| {
            let absolute = if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                std::env::current_dir().ok()?.join(dir)
            };
            let relative = canonical(&absolute)
                .strip_prefix(canonical(folder))
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some(if relative.is_empty() {
                relative
            } else {
                format!("{relative}/")
            })
        });
        Self { prefix }
    }

    /// Whether `path` (a project path) is the wiki's manifest, lock or a
    /// proposal.
    pub fn holds(&self, path: &str) -> bool {
        let Some(prefix) = &self.prefix else {
            return false;
        };
        path.strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest == MANIFEST || rest == LOCK || rest.starts_with("proposals/"))
    }
}

/// The areas of `repository`'s current code graph, largest first, at most
/// `max`, without the files of the wiki (`own`).
pub fn areas(
    db: &Connection,
    p: &Principal,
    repository: &str,
    max: usize,
    own: &Own,
) -> Result<Areas> {
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
            if prose(&path) || own.holds(&path) {
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
        let base = slug_of(&areas[k].hub);
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

/// A new article's name from `hub` that [`valid_slug`] accepts: its file
/// name of at most 80 characters, with no `..` and, once cut, no `.` at
/// either end.
fn slug_of(hub: &str) -> String {
    let mut slug = graphicle::wiki::safe_filename(hub, 80);
    while slug.contains("..") {
        slug = slug.replace("..", ".");
    }
    let slug = slug.trim_matches(['.', ' ']);
    // The index keeps its name: the caller numbers an area named after it.
    if valid_slug(slug) || slug.eq_ignore_ascii_case("index") {
        slug.to_string()
    } else {
        "area".to_string()
    }
}

/// The areas a wiki covers, named: every area that has an article in
/// `manifest` keeps it while the area exists, and the largest of the
/// others follow until there are `max` (a cap only on new articles).
/// Links between them are kept; links to areas left out are dropped.
pub fn wiki_areas(
    db: &Connection,
    p: &Principal,
    repository: &str,
    max: usize,
    manifest: &Manifest,
    own: &Own,
) -> Result<Areas> {
    let mut all = areas(db, p, repository, usize::MAX, own)?;
    assign_slugs(&mut all.areas, manifest);
    let mut keep: Vec<bool> = all
        .areas
        .iter()
        .map(|a| manifest.articles.contains_key(&a.slug))
        .collect();
    let mut count = keep.iter().filter(|k| **k).count();
    for k in keep.iter_mut() {
        if count >= max {
            break;
        }
        if !*k {
            *k = true;
            count += 1;
        }
    }
    let mut index = vec![None; keep.len()];
    let mut chosen = Vec::new();
    for (i, area) in all.areas.into_iter().enumerate() {
        if keep[i] {
            index[i] = Some(chosen.len());
            chosen.push(area);
        }
    }
    for area in &mut chosen {
        area.links = area
            .links
            .iter()
            .filter_map(|(other, edges)| index[*other].map(|n| (n, *edges)))
            .collect();
    }
    Ok(Areas {
        areas: chosen,
        thin: all.thin,
        left_out: keep.iter().filter(|k| !**k).count(),
    })
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
    /// The run settings the wiki keeps: `max_articles` and `budget`.
    pub settings: Value,
}

/// Whether `slug` can name an article: a plain file name in the wiki's
/// folder, as [`assign_slugs`] makes them (no separator, no `..`, not
/// absolute, not the index).
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.chars().count() <= 100
        && slug == graphicle::wiki::safe_filename(slug, 1_000)
        && !slug.contains(['/', '\\'])
        && !slug.contains("..")
        && !Path::new(slug).is_absolute()
        && !slug.eq_ignore_ascii_case("index")
}

impl Manifest {
    /// The manifest in `dir`, if there is one. A manifest that names an
    /// article by anything but a plain file name ([`valid_slug`]) is
    /// refused: its names become paths.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(MANIFEST);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| Error::bad(format!("{} is not a wiki manifest", path.display())))?;
        let Some(listed) = value["articles"].as_object() else {
            return Err(Error::bad(format!(
                "{} is not a wiki manifest",
                path.display()
            )));
        };
        if let Some(bad) = listed.keys().find(|slug| !valid_slug(slug)) {
            return Err(Error::bad(format!(
                "{} names an article {bad:?}, which is not a plain file name: remove the manifest or that entry",
                path.display()
            )));
        }
        let articles = listed.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let settings = match &value["settings"] {
            Value::Object(_) => value["settings"].clone(),
            _ => json!({}),
        };
        Ok(Some(Self { articles, settings }))
    }

    /// Write the manifest in `dir`, whole or not at all.
    pub fn save(&self, dir: &Path, project: &str) -> Result<()> {
        let articles: Map<String, Value> = self
            .articles
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let settings = match &self.settings {
            Value::Object(_) => self.settings.clone(),
            _ => json!({}),
        };
        let value =
            json!({"rules":RULES,"project":project,"settings":settings,"articles":articles});
        write_if_changed(
            &dir.join(MANIFEST),
            &format!("{}\n", serde_json::to_string_pretty(&value)?),
        )?;
        Ok(())
    }
}

/// Write `text` to `path` through a new temporary file beside it, renamed
/// into place: a symbolic link at `path` is replaced, never followed.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
    };
    // A file left by an earlier run with the same process id, or a link
    // put there, is removed (the link itself, not what it names) rather
    // than written through.
    let mut file = match create() {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&temporary)?;
            create()?
        }
        other => other?,
    };
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
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

/// Whether `text` is a file of the wiki this program wrote: an article or
/// the index, which start with the same front matter.
/// A checkout that converts line ends (`core.autocrlf`) gives it `\r\n`.
pub fn is_article(text: &str) -> bool {
    text.strip_prefix("---")
        .map(|rest| rest.strip_prefix('\r').unwrap_or(rest))
        .and_then(|rest| rest.strip_prefix('\n'))
        .is_some_and(|rest| rest.starts_with(&format!("{MARK}: ")))
}

/// Whether this program may write `path`: it does not exist, or it is a
/// file of the wiki this program wrote.
pub fn ours(path: &Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => std::str::from_utf8(&bytes).is_ok_and(is_article),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// The lock of a run that writes the wiki in `dir`, held until it is
/// dropped; `None` when another run holds it. A symbolic link at the lock's
/// path is refused, not followed.
pub fn lock(dir: &Path) -> Result<Option<std::fs::File>> {
    let path = dir.join(LOCK);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::bad(format!(
            "{} is a symbolic link: remove it",
            path.display()
        )));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
    let file = options.open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Model text made safe to render as Markdown prose: `\\`, `[`, `]`, `(`,
/// `)`, `<` and `>` are escaped outside code spans, so a reply cannot add
/// Markdown links, images or HTML.
pub fn escape_markdown(text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    // An odd count of backticks leaves the last one unpaired: its text is
    // prose.
    let paired = if parts.len().is_multiple_of(2) {
        parts.len() - 1
    } else {
        parts.len()
    };
    let mut out = String::with_capacity(text.len());
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 && i < paired {
            out.push_str(part);
            continue;
        }
        for c in part.chars() {
            if matches!(c, '\\' | '[' | ']' | '(' | ')' | '<' | '>') {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
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

/// The lines of each file a request showed the model: a citation must fall
/// in them.
#[derive(Clone, Debug, Default)]
pub struct Shown(BTreeMap<String, Vec<(usize, usize)>>);

impl Shown {
    /// Lines `start..=end` of `path`.
    pub fn add(&mut self, path: &str, start: usize, end: usize) {
        if start == 0 || end < start {
            return;
        }
        let ranges = self.0.entry(path.to_string()).or_default();
        ranges.push((start, end));
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for &(a, b) in ranges.iter() {
            match merged.last_mut() {
                Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        *ranges = merged;
    }

    /// The excerpts of a code pack.
    pub fn pack(&mut self, pack: &Value) {
        for section in pack["sections"].as_array().into_iter().flatten() {
            for chunk in section["chunks"].as_array().into_iter().flatten() {
                if let (Some(path), Some(start), Some(end)) = (
                    chunk["path"].as_str(),
                    chunk["start_line"].as_u64(),
                    chunk["end_line"].as_u64(),
                ) {
                    self.add(path, start as usize, end as usize);
                }
            }
        }
    }

    /// A passage of `text` from line `line` of `path`.
    pub fn passage(&mut self, path: &str, line: usize, text: &str) {
        let lines = text.lines().count().max(1);
        self.add(path, line, line + lines - 1);
    }

    /// Whether lines `line..=end` of `path` were shown.
    pub fn holds(&self, path: &str, line: usize, end: usize) -> bool {
        self.0
            .get(path)
            .is_some_and(|ranges| ranges.iter().any(|(a, b)| *a <= line && end <= *b))
    }
}

/// Counts of what the check of a reply dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub paragraphs: usize,
    pub citations: usize,
}

/// The reply with every citation checked: a citation outside the lines
/// the request showed (`shown`) or that does not hold in the working tree
/// is dropped, and so is a paragraph left with none. Text is made one line
/// per paragraph.
pub fn check_reply(reply: &Value, tree: &mut Tree, shown: &Shown) -> (Value, Dropped) {
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
                if !shown.holds(&citation.path, citation.line, citation.end)
                    || tree
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
        "" => slug.to_string(),
        title => escape_markdown(title),
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
        text.push_str(&format!("{}\n\n", escape_markdown(s(reply, "summary"))));
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
                escape_markdown(s(&paragraph, "text")),
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
            text.push_str(&format!(
                "- {}\n",
                escape_markdown(question.as_str().unwrap_or(""))
            ));
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
        "---\n{MARK}: {RULES}\nkind: index\n---\n\n# {}: project wiki\n\nWritten by a model from the project's code, docs and reviewed memories; every paragraph of an article cites file and line. `graphicle wiki update` keeps it current and `graphicle wiki check` lists what went stale.\n\n",
        escape_markdown(project)
    );
    for (slug, record) in entries {
        let title = match s(&record["reply"], "title") {
            "" => slug.clone(),
            title => escape_markdown(title),
        };
        let summary = escape_markdown(s(&record["reply"], "summary"));
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
    let Ok(url) = url::Url::parse(profile.base_url.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The output limit of a call for `profile` whose reply needs `base`
/// tokens at most: [`REASONING_FACTOR`] times as many for a profile that
/// sets a reasoning effort, whose reasoning counts as output.
pub fn output_limit(profile: &model::Profile, base: usize) -> usize {
    let reasons = profile
        .reasoning_effort
        .as_deref()
        .is_some_and(|effort| !matches!(effort, "none" | "minimal"));
    let wanted = if reasons {
        base * REASONING_FACTOR
    } else {
        base
    };
    profile.output_limit.min(wanted)
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

    /// Count a failed call: its cost in the spend log `entry` (a reply cut
    /// at the output limit, for one, is billed), else its worst case.
    pub fn add_failed(&mut self, entry: &Value, worst: f64) {
        self.spent += entry["cost_usd"].as_f64().unwrap_or(worst);
    }

    /// Count a finished call by its usage, or at `worst`, its worst case,
    /// when the server reported no usage (not every OpenAI-compatible
    /// server does).
    pub fn add(&mut self, done: &model::Completion, worst: f64) {
        let Some(price) = &self.price else {
            return;
        };
        self.spent += match (done.input_tokens, done.output_tokens) {
            (Some(input), Some(output)) => {
                price.cost(input, done.cached_tokens.unwrap_or(0), output)
            }
            _ => worst,
        };
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
    if update && !dir.join(MANIFEST).exists() {
        return Err(Error::bad(format!(
            "No wiki in {}: graphicle wiki build writes one",
            dir.display()
        )));
    }
    // A run that writes holds the folder's lock before it reads the
    // manifest, so two runs never write it, or spend, at once.
    let _lock = if args.dry_run {
        None
    } else {
        // A manifest this program would refuse stops the run before it
        // creates the folder or the lock.
        Manifest::load(&dir)?;
        std::fs::create_dir_all(&dir)?;
        match lock(&dir)? {
            Some(file) => Some(file),
            None => {
                return Err(Error::bad(format!(
                    "Another graphicle wiki run is writing {}; this one stops",
                    dir.display()
                )));
            }
        }
    };
    let mut manifest = Manifest::load(&dir)?.unwrap_or_default();
    let own = Own::of(&target, &dir);
    // The wiki keeps the settings it was written with, so the commit
    // hook's runs, which name none, cover the same areas.
    let max_articles = args
        .max_articles
        .or_else(|| manifest.settings["max_articles"].as_u64())
        .unwrap_or(DEFAULT_MAX_ARTICLES)
        .clamp(1, 500);
    let pack_budget = args
        .budget
        .or_else(|| manifest.settings["budget"].as_u64())
        .unwrap_or(DEFAULT_BUDGET)
        .clamp(1_000, 200_000);
    let (found, all_inputs) = store.authorized_read(
        &target.session.workspace,
        &target.session.capability,
        "read",
        |db, p| {
            let found = wiki_areas(
                db,
                p,
                &target.project,
                max_articles as usize,
                &manifest,
                &own,
            )?;
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
        budget: Some(pack_budget as usize),
        layout: Some("standard".into()),
        prefix_share: None,
        json: !human,
    };
    let profiled = long_context::profile(&run);
    let ratio = profiled
        .as_ref()
        .ok()
        .and_then(|(name, profile)| long_context::calibration(store, name, profile));
    if args.dry_run {
        let settings = (max_articles, pack_budget, ratio);
        return dry_run(&args, settings, &dir, &found, &plan, profiled.ok(), human);
    }
    let (name, mut profile) = profiled?;
    profile.output_limit = output_limit(&profile, OUTPUT_TOKENS);
    let mut budget = Budget::new(&name, &profile, args.max_cost)?;
    if target.folder.is_none() {
        return Err(Error::bad(
            "This project has no working tree here to check the articles' citations against: run graphicle wiki in its synced folder",
        ));
    }
    manifest.settings = json!({"max_articles":max_articles,"budget":pack_budget});
    let commit = target
        .folder
        .as_deref()
        .and_then(crate::code_pack::git_head)
        .unwrap_or_default();
    let mut tree = Tree::new(target.folder.clone());
    let schema = article_schema();
    let mut written = Vec::new();
    let mut failed = Vec::new();
    let mut not_ours = Vec::new();
    let mut dropped = Dropped::default();
    let mut stopped: Option<String> = None;
    let mut left: Vec<String> = Vec::new();
    let mut in_a_row = 0usize;
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
    manifest.save(&dir, &target.name)?;
    for planned in plan.iter().filter(|p| p.write) {
        let area = &found.areas[planned.area];
        if stopped.is_some() {
            left.push(area.slug.clone());
            continue;
        }
        let path = dir.join(format!("{}.md", area.slug));
        if !ours(&path) {
            not_ours.push(path.clone());
            failed.push(json!({"area":area.slug,"error":format!(
                "{} exists and graphicle wiki did not write it: it is left as it is",
                path.display()
            )}));
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
            pack_budget as usize,
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
            crate::spend::worst_input(estimate, None, ratio),
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
            ratio,
            &mut |_| {},
        )
        .await;
        let (result, entry) = match sent {
            Ok(sent) => sent,
            // A cap refused the call: nothing more is sent.
            Err(error) => {
                stopped = Some(error.message);
                left.push(area.slug.clone());
                continue;
            }
        };
        let reply = match result {
            Ok(done) => {
                budget.add(&done, worst);
                match long_context::json_in(&done.text) {
                    Ok(reply) if reply.is_object() => Ok((done, reply)),
                    _ => Err("The model's article is not a JSON object".to_string()),
                }
            }
            Err(error) => {
                // A failed call can be billed: a reply cut at the output
                // limit is.
                budget.add_failed(&entry, worst);
                Err(error.message)
            }
        };
        let (done, reply) = match reply {
            Ok(found) => {
                in_a_row = 0;
                found
            }
            Err(error) => {
                failed.push(json!({"area":area.slug,"error":error}));
                in_a_row += 1;
                if in_a_row >= FAILURES_IN_A_ROW {
                    stopped = Some(format!(
                        "{in_a_row} calls in a row failed, the last with: {error}"
                    ));
                }
                continue;
            }
        };
        let mut shown = Shown::default();
        shown.pack(&pack);
        for passage in &planned.inputs.passages {
            shown.passage(
                s(passage, "path"),
                passage["line"].as_u64().unwrap_or(0) as usize,
                s(passage, "text"),
            );
        }
        let (checked, lost) = check_reply(&reply, &mut tree, &shown);
        dropped.paragraphs += lost.paragraphs;
        dropped.citations += lost.citations;
        let record = json!({"digest":planned.inputs.digest,"model":done.model,"profile":name,
            "written_at":crate::now(),"commit":commit,"files":area.files,
            "docs":planned.inputs.passages.iter().map(|p| format!("{}:{}", s(p,"path"), p["line"])).collect::<Vec<_>>(),
            "memories":planned.inputs.memories.iter().map(|m| json!({"id":m["id"],"title":m["title"]})).collect::<Vec<_>>(),
            "dropped":{"paragraphs":lost.paragraphs,"citations":lost.citations},"reply":checked});
        let text = render(&area.slug, &record, &links_of(area, &found.areas));
        write_atomic(&path, &text)?;
        manifest.articles.insert(area.slug.clone(), record);
        manifest.save(&dir, &target.name)?;
        written.push(area.slug.clone());
    }
    // Unchanged articles: their listed sections follow the areas as they
    // are now, without a model.
    let mut rerendered = Vec::new();
    for planned in plan.iter().filter(|p| !p.write) {
        let area = &found.areas[planned.area];
        let path = dir.join(format!("{}.md", area.slug));
        if let Some(record) = manifest.articles.get(&area.slug) {
            if !ours(&path) {
                not_ours.push(path);
                continue;
            }
            let text = render(&area.slug, record, &links_of(area, &found.areas));
            if write_if_changed(&path, &text)? {
                rerendered.push(area.slug.clone());
            }
        }
    }
    // The index lists the largest areas first, and areas of the same size
    // by name: the graph's own order of a tie can change from one sync to
    // the next.
    let mut ordered: Vec<&Area> = found.areas.iter().collect();
    ordered.sort_by(|a, b| {
        b.symbols
            .len()
            .cmp(&a.symbols.len())
            .then_with(|| a.slug.cmp(&b.slug))
    });
    let entries: Vec<(String, Value)> = ordered
        .iter()
        .filter_map(|a| {
            manifest
                .articles
                .get(&a.slug)
                .map(|r| (a.slug.clone(), r.clone()))
        })
        .collect();
    let index_text = index(&target.name, &entries, found.thin, found.left_out);
    let index_path = dir.join("index.md");
    let index_written = if entries.is_empty() {
        false
    } else if !ours(&index_path) {
        not_ours.push(index_path.clone());
        false
    } else {
        write_if_changed(&index_path, &index_text)?
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
        "not_replaced":not_ours,
        "dropped":{"paragraphs":dropped.paragraphs,"citations":dropped.citations},
        "areas":found.areas.len(),"thin_areas":found.thin,"left_out":found.left_out,
        "max_articles":max_articles,"budget":pack_budget,
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

/// `--dry-run`: the articles to write, their worst-case input and cost,
/// by the formula the run stops at: the estimate with the profile's
/// calibration and the same margin, and the run's output limit.
fn dry_run(
    args: &WriteArgs,
    (max_articles, pack_budget, ratio): (u64, u64, Option<f64>),
    dir: &Path,
    found: &Areas,
    plan: &[Planned],
    profile: Option<(String, model::Profile)>,
    human: bool,
) -> Result<()> {
    let output = profile
        .as_ref()
        .map_or(OUTPUT_TOKENS, |(_, p)| output_limit(p, OUTPUT_TOKENS)) as u64;
    let mut items = Vec::new();
    let mut total_input = 0u64;
    let mut total_cost = 0.0;
    for planned in plan {
        let area = &found.areas[planned.area];
        let rest = area_text(area, &found.areas, &planned.inputs);
        // The pack fills its budget at most; the rest is counted by the
        // estimator's rate.
        let estimate = pack_budget
            + crate::code_pack::estimate::tokens(
                WIKI_INSTRUCTIONS.len() + rest.len() + 32,
                crate::code_pack::estimate::PACK_TEXT_BYTES_PER_TOKEN,
            ) as u64;
        let input = crate::spend::worst_input(estimate, None, ratio);
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
        "max_articles":max_articles,"budget":pack_budget,
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
    let not_replaced: Vec<String> = list("not_replaced");
    if !not_replaced.is_empty() {
        text.push_str(&format!(
            "Not replaced: {} file(s) that graphicle wiki did not write ({})\n",
            not_replaced.len(),
            not_replaced.join(", ")
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

/// The wiki block's markers.
const HOOK_MARKERS: crate::git_hooks::Markers = crate::git_hooks::Markers {
    start: HOOK_START,
    end: HOOK_END,
};

/// What `wiki hook` does to a hook's text.
#[derive(Debug, PartialEq, Eq)]
pub enum HookChange {
    /// Write this text.
    Write(String),
    /// The hook holds nothing else: remove the file.
    Remove,
    /// Nothing to do.
    Unchanged,
}

/// `text` (a hook, `None` when there is none) with the wiki block for
/// `folder` set to `block`, or removed when `block` is `None`, by the rules
/// of the freshness hooks ([`crate::git_hooks::upsert_marked`]): a hook in
/// another language is refused, the block goes before a final `exit` or
/// `exec`, and a block without its end line is never matched, so the rest
/// of the hook stays.
pub fn hook_text(text: Option<&str>, folder: &Path, block: Option<&str>) -> Result<HookChange> {
    use crate::git_hooks::{self, Upserted};
    match block {
        Some(block) => match git_hooks::upsert_marked(HOOK_MARKERS, text, block, folder) {
            Upserted::Changed(new, _) => Ok(HookChange::Write(new)),
            Upserted::Unchanged => Ok(HookChange::Unchanged),
            Upserted::NotShell => Err(Error::bad(format!(
                "The post-commit hook is not a shell script, so graphicle leaves it alone; add this line to it yourself: graphicle wiki update --project {} --quiet >/dev/null 2>&1 &",
                git_hooks::quote(&folder.to_string_lossy())
            ))),
        },
        None => Ok(
            match text.and_then(|t| git_hooks::remove_marked(HOOK_MARKERS, t, Some(folder))) {
                None => HookChange::Unchanged,
                Some(new) if git_hooks::empty(&new) => HookChange::Remove,
                Some(new) => HookChange::Write(new),
            },
        ),
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
    match hook_text(existing.as_deref(), &folder, block.as_deref())? {
        HookChange::Write(text) => {
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
        HookChange::Remove => {
            std::fs::remove_file(&path)?;
            println!(
                "Removed the wiki block, and {}, which held nothing else.",
                path.display()
            );
        }
        HookChange::Unchanged if on => println!(
            "The post-commit hook already runs graphicle wiki update for {} ({}).",
            folder.display(),
            path.display()
        ),
        HookChange::Unchanged => println!("No wiki block in {}.", path.display()),
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
            "connections":[{"text":"Not shown.","citations":["a.py:3"]}],
            "docs_and_memories":[],"open_questions":["Why?",""]});
        // The request showed lines 1-2 and 2-3 of a.py in two excerpts,
        // which together hold a.py:1-3, and lines of b.py, which is not in
        // the tree.
        let mut shown = Shown::default();
        shown.add("a.py", 1, 2);
        shown.add("a.py", 2, 3);
        shown.add("b.py", 1, 5);
        let (checked, dropped) = check_reply(&reply, &mut tree, &shown);
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
            checked["connections"],
            json!([{"text":"Not shown.","citations":["a.py:3"]}])
        );
        assert_eq!(
            dropped,
            Dropped {
                paragraphs: 2,
                citations: 3
            }
        );
        // A citation of lines the request never showed is dropped, though
        // the file holds them.
        let mut narrow = Shown::default();
        narrow.add("a.py", 1, 1);
        let (checked, dropped) = check_reply(&reply, &mut tree, &narrow);
        assert_eq!(checked["purpose"], json!([]));
        assert_eq!(checked["key_parts"], json!([]));
        assert_eq!(dropped.paragraphs, 5);
    }

    #[test]
    fn a_pack_and_a_passage_show_their_lines() {
        let pack = json!({"sections":[{"chunks":[
            {"path":"a.py","start_line":10,"end_line":20,"text":""},
            {"path":null,"start_line":null,"end_line":null,"text":"header"}]}]});
        let mut shown = Shown::default();
        shown.pack(&pack);
        shown.passage("docs/a.md", 3, "one\ntwo\n");
        assert!(shown.holds("a.py", 10, 20));
        assert!(!shown.holds("a.py", 9, 10));
        assert!(shown.holds("docs/a.md", 4, 4));
        assert!(!shown.holds("docs/a.md", 5, 5));
        assert!(!shown.holds("b.py", 1, 1));
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
    fn the_wiki_s_own_files_are_not_code() {
        let own = Own {
            prefix: Some("docs/wiki/".into()),
        };
        assert!(own.holds("docs/wiki/manifest.json"));
        assert!(own.holds("docs/wiki/.graphicle-wiki.lock"));
        assert!(own.holds("docs/wiki/proposals/a.md-L3.patch"));
        assert!(!own.holds("docs/wiki/tool.py"));
        assert!(!own.holds("manifest.json"));
        let top = Own {
            prefix: Some(String::new()),
        };
        assert!(top.holds("manifest.json"));
        assert!(!top.holds("src/manifest.json"));
        assert!(!Own::default().holds("manifest.json"));
    }

    #[test]
    fn model_text_cannot_add_links_or_html() {
        assert_eq!(
            escape_markdown("See [x](http://e.com) <img> `Vec<T>` and a(b)"),
            "See \\[x\\]\\(http://e.com\\) \\<img\\> `Vec<T>` and a\\(b\\)"
        );
        // An unpaired backtick leaves the rest as prose.
        assert_eq!(escape_markdown("a ` [b]"), "a ` \\[b\\]");
        assert_eq!(escape_markdown("back\\slash"), "back\\\\slash");
        let record = json!({"digest":"d","model":"m","files":[],
            "reply":{"title":"[T](javascript:x)","summary":"<script>","purpose":[
                {"text":"![i](http://e.com/p.png)","citations":["a.py:1"]}],
                "key_parts":[],"connections":[],"docs_and_memories":[],"open_questions":["<b>?"]}});
        let text = render("t", &record, &[]);
        assert!(text.contains("# \\[T\\]\\(javascript:x\\)\n"), "{text}");
        assert!(text.contains("\\<script\\>\n"), "{text}");
        assert!(
            text.contains("!\\[i\\]\\(http://e.com/p.png\\) (`a.py:1`)"),
            "{text}"
        );
        assert!(text.contains("- \\<b\\>?\n"), "{text}");
        let index = index("p", &[("t".into(), record)], 0, 0);
        assert!(is_article(&index), "{index}");
        assert!(
            index.contains("- [\\[T\\]\\(javascript:x\\)](t.md): \\<script\\>\n"),
            "{index}"
        );
    }

    #[test]
    fn a_manifest_names_articles_by_plain_file_names_only() {
        for slug in ["parser", "index_2", "Reader.read", "a_b-c"] {
            assert!(valid_slug(slug), "{slug}");
        }
        for slug in [
            "", "../x", "..", "a/b", "a\\b", "/etc/x", "index", "a..b", "a:b",
        ] {
            assert!(!valid_slug(slug), "{slug}");
        }
        let dir = tempfile::TempDir::new().unwrap();
        let save = |articles: Value| {
            std::fs::write(
                dir.path().join(MANIFEST),
                json!({"rules":RULES,"articles":articles}).to_string(),
            )
            .unwrap();
        };
        save(json!({"parser":{"files":["a.py"]}}));
        let loaded = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.articles.len(), 1);
        for bad in ["../../outside", "/tmp/abs", "sub/dir"] {
            save(json!({bad:{"files":["a.py"]}}));
            let error = Manifest::load(dir.path()).unwrap_err();
            assert!(error.message.contains("not a plain file name"), "{bad}");
        }
        std::fs::write(dir.path().join(MANIFEST), "{\"x\":1}").unwrap();
        assert!(Manifest::load(dir.path()).is_err());
        // The settings are kept.
        let manifest = Manifest {
            settings: json!({"max_articles":3,"budget":5000}),
            ..Manifest::default()
        };
        manifest.save(dir.path(), "p").unwrap();
        let loaded = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.settings["max_articles"], 3);
    }

    #[test]
    fn a_new_article_name_is_always_one_the_manifest_takes() {
        // Cut at 80 characters the first ends in "."; the second keeps "..".
        let hubs = [
            format!("{}.rest", "a".repeat(79)),
            "Reader..read".to_string(),
            "..".to_string(),
            "index".to_string(),
        ];
        let mut areas: Vec<Area> = hubs
            .iter()
            .enumerate()
            .map(|(i, hub)| area(hub, &[&format!("f{i}.py")]))
            .collect();
        assign_slugs(&mut areas, &Manifest::default());
        let mut manifest = Manifest::default();
        for area in &areas {
            assert!(valid_slug(&area.slug), "{:?}", area.slug);
            manifest
                .articles
                .insert(area.slug.clone(), json!({"files":area.files}));
        }
        assert_eq!(manifest.articles.len(), hubs.len());
        let dir = tempfile::TempDir::new().unwrap();
        manifest.save(dir.path(), "p").unwrap();
        let loaded = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.articles.len(), hubs.len());
    }

    #[test]
    fn an_article_checked_out_with_crlf_line_ends_is_ours() {
        let text = render("a", &json!({"reply":{}}), &[]);
        let crlf = text.replace('\n', "\r\n");
        assert!(is_article(&text));
        assert!(is_article(&crlf));
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, &crlf).unwrap();
        assert!(ours(&path));
        assert!(!is_article("---\r\r\ngraphicle_wiki: 1\n"));
        assert!(!is_article("---\ntitle: x\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_lock_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(LOCK)).unwrap();
        assert!(lock(dir.path()).is_err());
        assert!(!elsewhere.exists());
    }

    #[test]
    fn a_finished_call_with_no_usage_counts_at_its_worst_case() {
        let profile: model::Profile = serde_json::from_value(json!({"protocol":"chat",
            "base_url":"https://api.example.com/v1","model":"m","input_limit":8000,"output_limit":4000,
            "price":{"input":1.0,"output":2.0}}))
        .unwrap();
        let mut budget = Budget::new("m", &profile, 1.0).unwrap();
        let done = |input: Option<u64>, output: Option<u64>| model::Completion {
            text: String::new(),
            input_tokens: input,
            output_tokens: output,
            cached_tokens: None,
            reasoning_tokens: None,
            elapsed_ms: 0,
            model: "m".into(),
        };
        budget.add(&done(None, None), 0.4);
        assert!((budget.spent - 0.4).abs() < 1e-12);
        budget.add(&done(Some(100_000), None), 0.4);
        assert!((budget.spent - 0.8).abs() < 1e-12);
        assert!(!budget.fits(0.4));
        // Reported usage counts at its price: 0.1 + 0.02.
        budget.add(&done(Some(100_000), Some(10_000)), 0.4);
        assert!((budget.spent - 0.92).abs() < 1e-12);
    }

    #[test]
    fn only_files_this_program_wrote_are_replaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.md");
        assert!(ours(&path));
        std::fs::write(&path, "# My notes\n").unwrap();
        assert!(!ours(&path));
        std::fs::write(&path, render("a", &json!({"reply":{}}), &[])).unwrap();
        assert!(ours(&path));
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(!ours(&path));
    }

    #[test]
    fn one_run_at_a_time_writes_a_wiki() {
        let dir = tempfile::TempDir::new().unwrap();
        let held = lock(dir.path()).unwrap();
        assert!(held.is_some());
        assert!(lock(dir.path()).unwrap().is_none());
        drop(held);
        assert!(lock(dir.path()).unwrap().is_some());
    }

    #[test]
    fn a_failed_call_is_counted_at_its_logged_cost_else_its_worst_case() {
        let mut profile: model::Profile = serde_json::from_value(json!({"protocol":"responses",
            "base_url":"https://api.example.com/v1","model":"m","input_limit":8000,"output_limit":32000,
            "price":{"input":1.0,"output":2.0}}))
        .unwrap();
        let mut budget = Budget::new("m", &profile, 1.0).unwrap();
        budget.add_failed(&json!({"cost_usd":0.25,"status":"max_output_tokens"}), 0.5);
        budget.add_failed(&json!({"cost_usd":null,"status":"error"}), 0.5);
        assert!((budget.spent - 0.75).abs() < 1e-12);
        assert_eq!(output_limit(&profile, OUTPUT_TOKENS), OUTPUT_TOKENS);
        profile.reasoning_effort = Some("high".into());
        assert_eq!(output_limit(&profile, OUTPUT_TOKENS), 16_000);
        profile.reasoning_effort = Some("minimal".into());
        assert_eq!(output_limit(&profile, OUTPUT_TOKENS), OUTPUT_TOKENS);
        profile.reasoning_effort = Some("high".into());
        profile.output_limit = 6_000;
        assert_eq!(output_limit(&profile, OUTPUT_TOKENS), 6_000);
    }

    #[test]
    fn the_hook_block_is_added_replaced_and_removed() {
        let folder = Path::new("/w/p");
        let block = hook_block(folder, Path::new("/bin/graphicle"), None, 0.5, None).unwrap();
        assert!(block.contains("wiki update --project '/w/p' --max-cost 0.5 --quiet"));
        let written = |change: HookChange| match change {
            HookChange::Write(text) => text,
            other => panic!("{other:?}"),
        };
        let added = written(hook_text(None, folder, Some(&block)).unwrap());
        assert!(added.starts_with("#!/bin/sh\n# graphicle-wiki-start /w/p\n"));
        assert_eq!(
            hook_text(Some(&added), folder, Some(&block)).unwrap(),
            HookChange::Unchanged
        );
        // A hook that holds nothing else is removed, not left as `#!/bin/sh`.
        assert_eq!(
            hook_text(Some(&added), folder, None).unwrap(),
            HookChange::Remove
        );
        let other = written(hook_text(Some("#!/bin/sh\necho hi\n"), folder, Some(&block)).unwrap());
        assert!(other.starts_with("#!/bin/sh\necho hi\n\n# graphicle-wiki-start"));
        let changed = hook_block(folder, Path::new("/bin/graphicle"), None, 0.25, None).unwrap();
        let replaced = written(hook_text(Some(&other), folder, Some(&changed)).unwrap());
        assert!(replaced.contains("--max-cost 0.25"));
        assert!(!replaced.contains("--max-cost 0.5 "));
        let removed = written(hook_text(Some(&replaced), folder, None).unwrap());
        assert_eq!(removed, "#!/bin/sh\necho hi\n");
        assert_eq!(
            hook_text(Some(&removed), folder, None).unwrap(),
            HookChange::Unchanged
        );
        // A hook that ends in `exit 0` gets the block before it, and loses
        // it again whole.
        let exits = "#!/bin/sh\necho hi\nexit 0\n";
        let before = written(hook_text(Some(exits), folder, Some(&block)).unwrap());
        assert!(
            before.ends_with("# graphicle-wiki-end\n\nexit 0\n"),
            "{before}"
        );
        assert_eq!(
            written(hook_text(Some(&before), folder, None).unwrap()),
            exits
        );
        // A hook in another language is left alone.
        let python = "#!/usr/bin/env python3\nprint(1)\n";
        let refused = hook_text(Some(python), folder, Some(&block)).unwrap_err();
        assert!(
            refused.message.contains("not a shell script"),
            "{}",
            refused.message
        );
        // A block whose end line is gone is not matched: off keeps every
        // line after it.
        let cut = format!(
            "#!/bin/sh\n{}\necho after\n",
            block.trim_end_matches("# graphicle-wiki-end")
        );
        assert_eq!(
            hook_text(Some(&cut), folder, None).unwrap(),
            HookChange::Unchanged
        );
        // The freshness block of `graphicle setup` is not the wiki's.
        let sync = "#!/bin/sh\n# graphicle-sync-start /w/p\n(\n  true\n)\n# graphicle-sync-end\n";
        assert_eq!(
            hook_text(Some(sync), folder, None).unwrap(),
            HookChange::Unchanged
        );
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

    #[test]
    fn this_machine_is_the_host_of_the_url() {
        let mut profile: model::Profile = serde_json::from_value(json!({"protocol":"openai_chat",
            "base_url":"http://127.0.0.1:8080/v1","model":"local","input_limit":8000,"output_limit":1000}))
        .unwrap();
        for (url, local) in [
            ("http://localhost:11434/v1", true),
            ("http://LOCALHOST/v1", true),
            ("http://127.0.0.2:8080/v1", true),
            ("http://[::1]:8080/v1", true),
            ("http://[2001:db8::1]:8080/v1", false),
            ("http://localhost@api.example.com/v1", false),
            ("http://localhost:x@api.example.com/v1", false),
            ("http://127.0.0.1.example.com/v1", false),
            ("https://api.example.com/v1", false),
            ("file:///tmp/model", false),
            ("not a url", false),
        ] {
            profile.base_url = url.into();
            assert_eq!(on_this_machine(&profile), local, "{url}");
        }
    }
}
