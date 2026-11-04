//! `graphicle wiki check`: the health checks of a project's docs, memories
//! and wiki.
//!
//! Deterministic first, with no model:
//! - **stale doc mentions**: passages that name code changed after they
//!   were ([`crate::doc_stale`]);
//! - **stale memories**: reviewed memories withheld because what they cite
//!   changed;
//! - **wiki articles whose sources changed** (their inputs' digest differs
//!   from the one they were written from), articles of areas that are gone
//!   and articles missing from the folder;
//! - **broken citations**: `path:line` citations in the wiki's articles that
//!   no longer hold in the working tree;
//! - **knowledge gaps**: areas of the code with no doc passage, reviewed
//!   memory or article about them.
//!
//! Then, unless `--no-model`, a model pass under `--max-cost`: for each
//! stale doc mention and stale memory (at most `--max-items` of each), the
//! model reads the passage or memory and the current code it names and
//! answers `holds`, `outdated` (with a corrected text and its citations) or
//! `unsure`. Results are proposals: a doc's correction is written as a
//! patch in the wiki's `proposals/` folder for you to read and apply, and
//! a memory's goes to the approval queue as a draft (`memory.propose`).
//! Graphicle never changes your files. One more call suggests articles and
//! questions for the knowledge gaps. No web search.
use crate::{
    Result,
    long_context::{self, Request, Run, Tree},
    model,
    project_wiki::{self, Budget, Manifest, Target},
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

/// The options of `wiki check`.
#[derive(clap::Args, Clone)]
pub struct Args {
    /// The project: an id, a key or a synced folder (default: this folder's
    /// project, else the last synced one).
    #[arg(long)]
    pub project: Option<String>,
    /// The wiki's folder [default: .graphicle/wiki/written in the project's
    /// folder].
    #[arg(long, value_name = "DIR")]
    pub out: Option<std::path::PathBuf>,
    /// Run only the checks that need no model.
    #[arg(long)]
    pub no_model: bool,
    /// The configured model profile [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// The model pass's cap in US dollars at the profile's price.
    #[arg(long, value_name = "USD", default_value_t = 1.0)]
    pub max_cost: f64,
    /// Stale doc mentions, and stale memories, the model reads at most.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=200))]
    pub max_items: u64,
    /// Areas the checks read at most, the largest first.
    #[arg(long, value_name = "N", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=500))]
    pub max_areas: u64,
    /// Print the report as JSON (the default when the output is not a
    /// terminal).
    #[arg(long)]
    pub json: bool,
}

/// Listed items of each kind at most.
const LISTED: usize = 200;
/// Output tokens a check call may use at most.
const OUTPUT_TOKENS: usize = 2_000;
/// Lines of code or doc a check call shows around what it names.
const CODE_LINES: usize = 80;
/// Lines of context in a suggested patch.
const CONTEXT: usize = 3;

const VERDICT_INSTRUCTIONS: &str = "You check one passage of a project's docs, or one reviewed memory, against the project's current code, which changed after the passage or memory was written. You get the text, what changed, and the current code with each line's number before a tab. Return one JSON object that matches the schema. verdict: holds when the text is still true of the code, outdated when the code now says otherwise, unsure when the code shown cannot tell. reason: one or two sentences on why. corrected_text: for outdated, the whole passage or memory written again so that it holds, in the same style and format, changing only what the code contradicts; else an empty string. citations: the path:line or path:start-end of the code lines your verdict rests on.";

const GAPS_INSTRUCTIONS: &str = "You plan documentation for a codebase. You get areas of the code that have no doc, reviewed memory or wiki article about them: each with its files and most connected symbols. Return one JSON object that matches the schema: articles worth writing (a title, the area's id, and why it is worth it), and questions worth asking about these areas, the kind an engineer new to the code would ask. Be concrete and brief.";

fn verdict_schema() -> Value {
    json!({"type":"object","properties":{
        "verdict":{"type":"string","enum":["holds","outdated","unsure"]},
        "reason":{"type":"string"},"corrected_text":{"type":"string"},
        "citations":{"type":"array","items":{"type":"string"}}},
        "required":["verdict","reason","corrected_text","citations"],"additionalProperties":false})
}

fn gaps_schema() -> Value {
    json!({"type":"object","properties":{
        "articles":{"type":"array","items":{"type":"object","properties":{
            "title":{"type":"string"},"area":{"type":"string"},"why":{"type":"string"}},
            "required":["title","area","why"],"additionalProperties":false}},
        "questions":{"type":"array","items":{"type":"string"}}},
        "required":["articles","questions"],"additionalProperties":false})
}

/// The lines `[from, to]` (1-based) of `text`, numbered, at most
/// [`CODE_LINES`].
fn numbered(text: &str, from: usize, to: usize) -> String {
    text.lines()
        .enumerate()
        .skip(from.saturating_sub(1))
        .take(to.saturating_sub(from) + 1)
        .take(CODE_LINES)
        .map(|(i, line)| format!("{}\t{line}\n", i + 1))
        .collect()
}

/// A file of the working tree, if it is one.
fn read(folder: Option<&Path>, path: &str) -> Option<String> {
    let folder = folder?;
    if Path::new(path).is_absolute() || path.split('/').any(|part| part == "..") {
        return None;
    }
    std::fs::read(folder.join(path))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The passage (run of non-blank lines) of `text` that holds line `line`:
/// its first and last line.
pub fn passage_lines(text: &str, line: usize) -> Option<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let at = line.checked_sub(1)?;
    if at >= lines.len() || lines[at].trim().is_empty() {
        return None;
    }
    let mut first = at;
    while first > 0 && !lines[first - 1].trim().is_empty() {
        first -= 1;
    }
    let mut last = at;
    while last + 1 < lines.len() && !lines[last + 1].trim().is_empty() {
        last += 1;
    }
    Some((first + 1, last + 1))
}

/// A unified diff of `path` that replaces its lines `[first, last]` with
/// `corrected`, with [`CONTEXT`] lines around, as `git apply` reads it.
pub fn patch(path: &str, text: &str, first: usize, last: usize, corrected: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = first.saturating_sub(1).saturating_sub(CONTEXT);
    let end = (last + CONTEXT).min(lines.len());
    let before = &lines[start..first - 1];
    let old = &lines[first - 1..last];
    let after = &lines[last..end];
    let new: Vec<&str> = corrected.trim_end_matches('\n').lines().collect();
    let old_count = before.len() + old.len() + after.len();
    let new_count = before.len() + new.len() + after.len();
    let mut out = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{},{old_count} +{},{new_count} @@\n",
        start + 1,
        start + 1
    );
    for line in before {
        out.push_str(&format!(" {line}\n"));
    }
    for line in old {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new {
        out.push_str(&format!("+{line}\n"));
    }
    for line in after {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// The citations of a reply that hold in the working tree.
fn held(reply: &Value, tree: &mut Tree) -> Vec<long_context::Citation> {
    let mut out = Vec::new();
    for cited in reply["citations"].as_array().into_iter().flatten() {
        for citation in long_context::citations(cited.as_str().unwrap_or("")) {
            if tree
                .problem(&citation.path, citation.line, citation.end)
                .is_none()
                && !out.contains(&citation)
            {
                out.push(citation);
            }
        }
    }
    out
}

/// The deterministic part of the report.
fn deterministic(store: &Store, target: &Target, dir: &Path, args: &Args) -> Result<Value> {
    let session = &target.session;
    let mentions = crate::doc_links::stale_mentions(
        store,
        &session.workspace,
        &session.capability,
        &target.project,
        LISTED,
    )?;
    let manifest = Manifest::load(dir)?;
    let (memories, found, inputs) =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            let repository = crate::store::repo(db, p, &target.project)?;
            let memories = crate::memory::list(db, &repository, p, false)?;
            let mut found = project_wiki::areas(db, p, &target.project, args.max_areas as usize)?;
            project_wiki::assign_slugs(&mut found.areas, &manifest.clone().unwrap_or_default());
            let inputs = project_wiki::inputs(db, p, &target.project, &found.areas)?;
            Ok((memories, found, inputs))
        })?;
    let stale_memories: Vec<Value> = memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] == true)
        .take(LISTED)
        .map(|m| {
            json!({"id":m["id"],"revision":m["revision"],"kind":m["kind"],"title":m["title"],
            "reasons":m["stale_reasons"]})
        })
        .collect();
    let mut articles = json!({"status":"no_wiki"});
    let mut written: BTreeSet<String> = BTreeSet::new();
    if let Some(manifest) = &manifest {
        let mut changed = Vec::new();
        let mut gone = Vec::new();
        let mut missing = Vec::new();
        let mut broken = Vec::new();
        let mut tree = Tree::new(target.folder.clone());
        let current: Vec<&str> = found.areas.iter().map(|a| a.slug.as_str()).collect();
        for (slug, record) in &manifest.articles {
            written.insert(slug.clone());
            match current.iter().position(|c| c == slug) {
                Some(k) if record["digest"] != json!(inputs[k].digest) => {
                    changed.push(json!({"article":slug,"written_at":record["written_at"]}))
                }
                Some(_) => {}
                None => gone.push(json!({"article":slug})),
            }
            let path = dir.join(format!("{slug}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                missing.push(json!({"article":slug}));
                continue;
            };
            for citation in long_context::citations(&text) {
                if let Some(problem) = tree.problem(&citation.path, citation.line, citation.end) {
                    broken.push(json!({"article":slug,"citation":format!("{}:{}{}", citation.path, citation.line,
                        if citation.end > citation.line { format!("-{}", citation.end) } else { String::new() }),
                        "problem":problem}));
                }
            }
        }
        articles = json!({"status":"checked","articles":manifest.articles.len(),
            "sources_changed":changed,"areas_gone":gone,"missing":missing,"broken_citations":broken});
    }
    let gaps: Vec<Value> = found
        .areas
        .iter()
        .zip(&inputs)
        .filter(|(area, inputs)| {
            inputs.passages.is_empty()
                && inputs.memories.is_empty()
                && !written.contains(&area.slug)
        })
        .map(|(area, _)| {
            let symbols: Vec<String> = area
                .symbols
                .iter()
                .take(6)
                .map(|s| format!("{} ({}:{})", s.label, s.path, s.line))
                .collect();
            let files: Vec<&String> = area.files.iter().take(20).collect();
            json!({"area":area.slug,"symbols":area.symbols.len(),"files":area.files.len(),
                "key_symbols":symbols,"some_files":files})
        })
        .collect();
    Ok(
        json!({"stale_mentions":mentions,"stale_memories":{"count":stale_memories.len(),"memories":stale_memories},
        "wiki":articles,"knowledge_gaps":{"count":gaps.len(),"areas":gaps},
        "areas":found.areas.len(),"thin_areas":found.thin,"left_out":found.left_out}),
    )
}

/// `graphicle wiki check`.
pub async fn check(store: &Store, config: &model::Config, args: Args) -> Result<()> {
    let target = project_wiki::target(store, config, args.project.clone())?;
    let (dir, _) = project_wiki::folder_of(&target, args.out.as_deref())?;
    let mut report = deterministic(store, &target, &dir, &args)?;
    report["wiki_folder"] = json!(dir);
    let human = crate::terminal::human(args.json);
    report["model_pass"] = if args.no_model {
        json!({"status":"not_requested"})
    } else {
        model_pass(store, config, &target, &dir, &args, &report, human).await?
    };
    if !human {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print!("{}", text(&report));
    Ok(())
}

/// One check call: the reply, or why there is none. `Ok(None)` when the
/// run's cap stops it.
#[allow(clippy::too_many_arguments)]
async fn ask(
    run: &Run<'_>,
    name: &str,
    profile: &model::Profile,
    budget: &mut Budget,
    pack: &Value,
    instructions: &str,
    prompt: &str,
    schema: &Value,
) -> Result<Option<std::result::Result<Value, String>>> {
    let user_of = |_: &Value| prompt.to_string();
    let request = Request {
        question: "",
        rest: prompt,
        instructions,
        user_of: &user_of,
        schema: Some(schema),
    };
    let estimate = long_context::request_estimate(pack, instructions, prompt);
    let worst = budget.worst(
        crate::spend::worst_input(estimate, None, None),
        profile.output_limit as u64,
    );
    if !budget.fits(worst) {
        return Ok(None);
    }
    let (result, _) = match long_context::send(
        run,
        "wiki check",
        name,
        profile,
        pack,
        &request,
        None,
        None,
        &mut |_| {},
    )
    .await
    {
        Ok(sent) => sent,
        // A cap of the profile refused the call.
        Err(_) => return Ok(None),
    };
    Ok(Some(match result {
        Ok(done) => {
            budget.add(&done);
            match long_context::json_in(&done.text) {
                Ok(reply) if reply.is_object() => Ok(reply),
                _ => Err("The model's reply is not a JSON object".into()),
            }
        }
        Err(error) => Err(error.message),
    }))
}

/// The model pass over the deterministic report.
async fn model_pass(
    store: &Store,
    config: &model::Config,
    target: &Target,
    dir: &Path,
    args: &Args,
    report: &Value,
    human: bool,
) -> Result<Value> {
    let run = Run {
        store,
        session: &target.session,
        config,
        project: target.project.clone(),
        folder: target.folder.clone(),
        model: args.model.clone(),
        budget: None,
        layout: None,
        prefix_share: None,
        json: !human,
    };
    let (name, mut profile) = long_context::profile(&run)?;
    profile.output_limit = profile.output_limit.min(OUTPUT_TOKENS);
    let mut budget = Budget::new(&name, &profile, args.max_cost)?;
    let commit = target
        .folder
        .as_deref()
        .and_then(crate::code_pack::git_head);
    // The calls carry no pack: the spend log's project and commit only.
    let pack = json!({"project":target.name,"commit":commit,"tokens":0,"tokenizer":"estimate"});
    let folder = target.folder.as_deref();
    let mut tree = Tree::new(target.folder.clone());
    let schema = verdict_schema();
    let proposals = dir.join("proposals");
    let mut items = Vec::new();
    let mut stopped = false;
    let max = args.max_items as usize;
    for mention in report["stale_mentions"]["stale"]
        .as_array()
        .into_iter()
        .flatten()
        .take(max)
    {
        let document = s(mention, "document_id");
        let Some(path) = document.strip_prefix("file:") else {
            continue;
        };
        let line = mention["line"].as_u64().unwrap_or(0) as usize;
        let Some(doc) = read(folder, path) else {
            items.push(json!({"kind":"doc_mention","document":path,"line":line,"verdict":"skipped","reason":"not in the working tree"}));
            continue;
        };
        let Some((first, last)) = passage_lines(&doc, line) else {
            continue;
        };
        let passage: String = doc
            .lines()
            .skip(first - 1)
            .take(last - first + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let mut prompt =
            format!("# The passage: {path}:{first}-{last}\n{passage}\n\n# What changed\n");
        prompt.push_str(&changed_text(mention, folder));
        prompt.push_str("\nIs the passage still true of the code?\n");
        let Some(answer) = ask(
            &run,
            &name,
            &profile,
            &mut budget,
            &pack,
            VERDICT_INSTRUCTIONS,
            &prompt,
            &schema,
        )
        .await?
        else {
            stopped = true;
            break;
        };
        let mut item =
            json!({"kind":"doc_mention","document":path,"line":first,"mention":mention["mention"]});
        match answer {
            Err(error) => item["error"] = json!(error),
            Ok(reply) => {
                let cites = held(&reply, &mut tree);
                item["verdict"] = reply["verdict"].clone();
                item["reason"] = reply["reason"].clone();
                item["citations"] = json!(cites.iter().map(cite).collect::<Vec<_>>());
                let corrected = s(&reply, "corrected_text").trim();
                if reply["verdict"] == "outdated" && !corrected.is_empty() && !cites.is_empty() {
                    std::fs::create_dir_all(&proposals)?;
                    let file = proposals.join(format!(
                        "{}-L{first}.patch",
                        graphicle::wiki::safe_filename(&path.replace('/', "__"), 120)
                    ));
                    std::fs::write(&file, patch(path, &doc, first, last, corrected))?;
                    item["patch"] = json!(file);
                } else if reply["verdict"] == "outdated" {
                    item["note"] = json!("no correction kept: it cited no code that holds");
                }
            }
        }
        items.push(item);
    }
    let memories: Vec<Value> = report["stale_memories"]["memories"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !stopped {
        let full = store.authorized_read(
            &target.session.workspace,
            &target.session.capability,
            "read",
            |db, p| {
                let repository = crate::store::repo(db, p, &target.project)?;
                crate::memory::list(db, &repository, p, false)
            },
        )?;
        for listed in memories.iter().take(max) {
            let Some(memory) = full.iter().find(|m| m["id"] == listed["id"]) else {
                continue;
            };
            let prompt = memory_prompt(memory, folder);
            let Some(answer) = ask(
                &run,
                &name,
                &profile,
                &mut budget,
                &pack,
                VERDICT_INSTRUCTIONS,
                &prompt,
                &schema,
            )
            .await?
            else {
                stopped = true;
                break;
            };
            let mut item =
                json!({"kind":"memory","memory_id":memory["id"],"title":memory["title"]});
            match answer {
                Err(error) => item["error"] = json!(error),
                Ok(reply) => {
                    let cites = held(&reply, &mut tree);
                    item["verdict"] = reply["verdict"].clone();
                    item["reason"] = reply["reason"].clone();
                    item["citations"] = json!(cites.iter().map(cite).collect::<Vec<_>>());
                    let corrected = s(&reply, "corrected_text").trim().to_string();
                    if reply["verdict"] == "outdated" && !corrected.is_empty() {
                        item["proposal"] =
                            propose(store, target, memory, &corrected, &reply, &cites, folder);
                    }
                }
            }
            items.push(item);
        }
    }
    let gaps: Vec<Value> = report["knowledge_gaps"]["areas"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut suggestions = json!({"status":"not_needed"});
    if !gaps.is_empty() && !stopped {
        let mut prompt = String::from("# Areas with no doc, memory or article\n");
        for gap in gaps.iter().take(20) {
            prompt.push_str(&format!(
                "\n## {}\nFiles: {}\nSymbols: {}\n",
                s(gap, "area"),
                gap["some_files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                gap["key_symbols"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        suggestions = match ask(
            &run,
            &name,
            &profile,
            &mut budget,
            &pack,
            GAPS_INSTRUCTIONS,
            &prompt,
            &gaps_schema(),
        )
        .await?
        {
            None => {
                stopped = true;
                json!({"status":"stopped"})
            }
            Some(Err(error)) => json!({"status":"error","error":error}),
            Some(Ok(reply)) => json!({"status":"done","articles":reply["articles"],
                "questions":reply["questions"]}),
        };
    }
    Ok(
        json!({"status":if stopped {"stopped_at_cost_cap"} else {"done"},"items":items,
        "gap_suggestions":suggestions,"model":profile.model,"profile":name,
        "cost_usd":(budget.spent * 1_000_000.0).round() / 1_000_000.0,"max_cost_usd":budget.max,
        "priced":budget.price.is_some(),"proposals_folder":proposals}),
    )
}

fn cite(citation: &long_context::Citation) -> String {
    if citation.end > citation.line {
        format!("{}:{}-{}", citation.path, citation.line, citation.end)
    } else {
        format!("{}:{}", citation.path, citation.line)
    }
}

/// What changed for a flagged mention: the definition as it is now, or that
/// it is gone.
fn changed_text(mention: &Value, folder: Option<&Path>) -> String {
    let name = s(mention, "mention");
    let was = &mention["was"];
    if mention["state"] == crate::doc_stale::MISSING {
        return format!(
            "The passage names `{name}`, which no longer exists: it was {} at {}:{} and was renamed or deleted.\n",
            s(was, "qualified_name"),
            s(was, "relpath"),
            was["line"]
        );
    }
    let now = &mention["now"];
    let path = s(now, "relpath");
    let line = now["line"].as_u64().unwrap_or(1) as usize;
    let end = now["end_line"]
        .as_u64()
        .map(|e| e as usize)
        .unwrap_or(line + CODE_LINES - 1)
        .max(line);
    let code = read(folder, path)
        .map(|text| numbered(&text, line, end))
        .unwrap_or_default();
    format!(
        "The passage names `{name}` ({}), whose code changed after the passage was written. Its current code, {path}:{line}-{end}:\n{code}",
        s(now, "qualified_name")
    )
}

/// A stale memory's prompt: its text, why it is withheld and the current
/// code of the files it depends on.
fn memory_prompt(memory: &Value, folder: Option<&Path>) -> String {
    let mut prompt = format!(
        "# The memory: {} ({})\n{}\n\n# What changed\nIt is withheld because: {}\n",
        s(memory, "title"),
        s(memory, "kind"),
        s(memory, "body"),
        memory["stale_reasons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; ")
    );
    let mut paths: BTreeSet<String> = BTreeSet::new();
    if let Some(deps) = memory["data"]["dependencies"].as_object() {
        paths.extend(deps.keys().cloned());
    }
    for entry in memory["data"]["symbols"].as_array().into_iter().flatten() {
        if let Some(path) = entry["identity"]["relpath"]
            .as_str()
            .or_else(|| entry["relpath"].as_str())
        {
            paths.insert(path.to_string());
        }
    }
    for path in paths.iter().take(4) {
        if let Some(text) = read(folder, path) {
            prompt.push_str(&format!(
                "\n# Current code: {path}\n{}",
                numbered(&text, 1, CODE_LINES)
            ));
        }
    }
    prompt.push_str("\nIs the memory still true of the code?\n");
    prompt
}

/// A memory's correction as a draft in the approval queue
/// (`memory.propose`), citing the code lines the verdict rests on; the
/// proposal's id, or why there is none.
fn propose(
    store: &Store,
    target: &Target,
    memory: &Value,
    corrected: &str,
    reply: &Value,
    cites: &[long_context::Citation],
    folder: Option<&Path>,
) -> Value {
    let mut evidence = Vec::new();
    for citation in cites.iter().take(10) {
        let Some(text) = read(folder, &citation.path) else {
            continue;
        };
        let quote: String = text
            .lines()
            .skip(citation.line - 1)
            .take(citation.end - citation.line + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let quote = quote.trim();
        if quote.chars().count() >= 8 && quote.len() <= 4_000 {
            evidence.push(json!({"source_id":format!("file:{}", citation.path),"quote":quote}));
        }
    }
    if evidence.is_empty() {
        return json!({"status":"not_proposed","reason":"no cited code line holds a quote of 8 characters or more"});
    }
    let reason: String = format!(
        "graphicle wiki check: the reviewed memory {} may be outdated. {}",
        s(memory, "id"),
        s(reply, "reason")
    )
    .chars()
    .take(1_900)
    .collect();
    let key = format!(
        "wiki-check:{}:{}:{}",
        s(memory, "id"),
        memory["revision"],
        crate::digest(&json!(corrected))
    );
    let args = json!({"repository_id":target.project,"kind":memory["kind"],"title":memory["title"],
        "body":corrected,"reason":reason,"evidence":evidence,"idempotency_key":key});
    match store.project_mutate(
        &target.session.workspace,
        &target.session.capability,
        "memory.propose",
        &args,
    ) {
        Ok(result) => {
            json!({"status":"proposed","memory_id":result["memory_id"],"state":result["state"]})
        }
        Err(error) => json!({"status":"not_proposed","reason":error.message}),
    }
}

/// The readable report.
pub fn text(report: &Value) -> String {
    let mut out = String::new();
    let mentions = &report["stale_mentions"];
    out.push_str(&format!(
        "Stale doc mentions: {}\n",
        mentions["count"].as_u64().unwrap_or(0)
    ));
    for item in mentions["stale"].as_array().into_iter().flatten() {
        let path = s(item, "document_id").trim_start_matches("file:");
        let what = if item["state"] == crate::doc_stale::MISSING {
            "no longer exists".to_string()
        } else {
            format!("changed ({})", s(&item["now"], "relpath"))
        };
        out.push_str(&format!(
            "  {path}:{} names `{}`, which {what}\n",
            item["line"],
            s(item, "mention")
        ));
    }
    let memories = &report["stale_memories"];
    out.push_str(&format!(
        "Stale memories: {}\n",
        memories["count"].as_u64().unwrap_or(0)
    ));
    for item in memories["memories"].as_array().into_iter().flatten() {
        out.push_str(&format!("  {} ({})\n", s(item, "title"), s(item, "id")));
    }
    let wiki = &report["wiki"];
    if wiki["status"] == "no_wiki" {
        out.push_str("Wiki: none yet (graphicle wiki build writes one)\n");
    } else {
        let count = |key: &str| wiki[key].as_array().map_or(0, Vec::len);
        out.push_str(&format!(
            "Wiki: {} article(s); {} with changed sources, {} of areas that are gone, {} missing, {} broken citation(s)\n",
            wiki["articles"],
            count("sources_changed"),
            count("areas_gone"),
            count("missing"),
            count("broken_citations")
        ));
        for item in wiki["sources_changed"].as_array().into_iter().flatten() {
            out.push_str(&format!("  sources changed: {}\n", s(item, "article")));
        }
        for item in wiki["areas_gone"].as_array().into_iter().flatten() {
            out.push_str(&format!("  area gone: {}\n", s(item, "article")));
        }
        for item in wiki["broken_citations"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "  {}: `{}` {}\n",
                s(item, "article"),
                s(item, "citation"),
                s(item, "problem")
            ));
        }
    }
    let gaps = &report["knowledge_gaps"];
    out.push_str(&format!(
        "Knowledge gaps: {} area(s) with no doc, memory or article\n",
        gaps["count"].as_u64().unwrap_or(0)
    ));
    for item in gaps["areas"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} ({} symbols, {} files)\n",
            s(item, "area"),
            item["symbols"],
            item["files"]
        ));
    }
    let pass = &report["model_pass"];
    if pass["status"] == "not_requested" {
        return out;
    }
    out.push_str(&format!(
        "\nModel pass ({}, {}): {}\n",
        s(pass, "profile"),
        s(pass, "model"),
        s(pass, "status")
    ));
    for item in pass["items"].as_array().into_iter().flatten() {
        let what = if item["kind"] == "memory" {
            format!("memory {}", s(item, "title"))
        } else {
            format!("{}:{}", s(item, "document"), item["line"])
        };
        let verdict = item["verdict"].as_str().unwrap_or("error");
        let mut line = format!("  {what}: {verdict}");
        if let Some(path) = item["patch"].as_str() {
            line.push_str(&format!("; suggested patch {path}"));
        }
        if item["proposal"]["status"] == "proposed" {
            line.push_str(&format!(
                "; draft {} awaits approval",
                s(&item["proposal"], "memory_id")
            ));
        }
        if let Some(error) = item["error"].as_str() {
            line.push_str(&format!(" ({error})"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    let suggestions = &pass["gap_suggestions"];
    for article in suggestions["articles"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  suggested article: {} ({}): {}\n",
            s(article, "title"),
            s(article, "area"),
            s(article, "why")
        ));
    }
    for question in suggestions["questions"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  question worth asking: {}\n",
            question.as_str().unwrap_or("")
        ));
    }
    out.push_str(&format!(
        "Cost: {} at the configured price, of --max-cost {}. Nothing was changed: patches and drafts await you.\n",
        if pass["priced"] == true {
            crate::spend::usd(pass["cost_usd"].as_f64().unwrap_or(0.0))
        } else {
            "not priced".into()
        },
        crate::spend::usd(pass["max_cost_usd"].as_f64().unwrap_or(0.0))
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passage_is_its_run_of_lines_and_its_patch_applies_with_context() {
        let doc = "# Guide\n\nOne.\nTwo `f`.\n\nLast.\n";
        assert_eq!(passage_lines(doc, 4), Some((3, 4)));
        assert_eq!(passage_lines(doc, 2), None);
        assert_eq!(passage_lines(doc, 99), None);
        let diff = patch("docs/guide.md", doc, 3, 4, "One.\nTwo `g`, then three.\n");
        assert_eq!(
            diff,
            "--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1,6 +1,6 @@\n # Guide\n \n-One.\n-Two `f`.\n+One.\n+Two `g`, then three.\n \n Last.\n"
        );
    }

    #[test]
    fn numbered_lines_start_at_their_line() {
        assert_eq!(numbered("a\nb\nc\n", 2, 3), "2\tb\n3\tc\n");
        assert_eq!(numbered("a\n", 1, 9), "1\ta\n");
    }

    #[test]
    fn the_report_reads_as_text() {
        let report = json!({"stale_mentions":{"count":1,"stale":[{"document_id":"file:docs/a.md","line":3,
            "mention":"parse","state":"changed","now":{"relpath":"src/p.py"}}]},
            "stale_memories":{"count":0,"memories":[]},"wiki":{"status":"no_wiki"},
            "knowledge_gaps":{"count":1,"areas":[{"area":"io","symbols":5,"files":2}]},
            "model_pass":{"status":"not_requested"}});
        let out = text(&report);
        assert!(out.contains("docs/a.md:3 names `parse`, which changed (src/p.py)"));
        assert!(out.contains("Wiki: none yet"));
        assert!(out.contains("  io (5 symbols, 2 files)"));
        assert!(!out.contains("Model pass"));
    }

    #[test]
    fn a_missing_definition_says_so() {
        let mention = json!({"mention":"gone","state":"missing","was":{"qualified_name":"m.gone","relpath":"m.py","line":4}});
        assert!(
            changed_text(&mention, None)
                .contains("`gone`, which no longer exists: it was m.gone at m.py:4")
        );
    }

    #[test]
    fn a_verdict_reply_must_cite_code_that_holds() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.py"), "x\ny\n").unwrap();
        let mut tree = Tree::new(Some(dir.path().to_path_buf()));
        let found = held(
            &json!({"citations":["a.py:2","a.py:5","b.py:1"]}),
            &mut tree,
        );
        assert_eq!(found.iter().map(cite).collect::<Vec<_>>(), ["a.py:2"]);
    }
}
