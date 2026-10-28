//! `graphicle ask --pack` and `graphicle plan`: one model call over a pack
//! of the project's code ([`crate::code_pack`]) for a model with a long
//! context window (Muse Spark through the Meta Model API, or any
//! OpenAI-compatible server such as Muse Spark Open on vLLM or SGLang), then
//! every cited `path:line` checked against the working tree.
//!
//! The pack fills the profile's input limit less [`RESERVE`] (or `--budget`);
//! the question follows it. Tokens in and out are the provider's usage
//! counts; a cost is shown only from the profile's configured price.

use crate::local::Session;
use crate::store::Store;
use crate::{Error, Result, model};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What `ask --pack` tells the model: the same words for every question
/// (no date, no question), so that a provider's prompt cache can match them
/// and the pack's repository part after them.
pub const ASK_INSTRUCTIONS: &str = "You answer questions about a code repository from a pack of its code. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included; a pack in the cache layout has a repository part first and a question part after it. Code lines that start with a number and a tab carry their line number. Answer from the pack only. Cite each claim about the code as path:line or path:start-end, with the path exactly as the pack shows it. Say plainly what the pack does not show and which files you would need to see; do not guess at code you cannot see.";

/// What `plan` tells the model (the same words for every request).
pub const PLAN_INSTRUCTIONS: &str = "You plan a change to a code repository from a pack of its code. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included; a pack in the cache layout has a repository part first and a question part after it. Code lines that start with a number and a tab carry their line number. Return the plan as one JSON object that matches the schema: a short summary; the steps in order; the files to edit, each with the line range to change (line numbers from the pack) and the reason; the new files with their purpose; the tests to add, update or run; and the risks. Edit only files the pack shows, with line ranges inside them, and list files that do not exist yet as new files. Use paths exactly as the pack shows them.";

/// Tokens kept out of a pack that fills a profile's input limit: the
/// instructions, the question and the estimate's slack.
pub const RESERVE: usize = 4_000;

/// The plan's JSON schema.
pub fn plan_schema() -> Value {
    json!({"type":"object","properties":{
        "summary":{"type":"string"},
        "steps":{"type":"array","items":{"type":"string"}},
        "edits":{"type":"array","items":{"type":"object","properties":{
            "path":{"type":"string"},"start_line":{"type":"integer"},"end_line":{"type":"integer"},
            "reason":{"type":"string"}}}},
        "new_files":{"type":"array","items":{"type":"object","properties":{
            "path":{"type":"string"},"purpose":{"type":"string"}}}},
        "tests":{"type":"array","items":{"type":"object","properties":{
            "action":{"type":"string","enum":["add","update","run"]},"path":{"type":"string"},
            "what":{"type":"string"}}}},
        "risks":{"type":"array","items":{"type":"string"}}}})
}

/// What a long-context command runs on.
pub struct Run<'a> {
    pub store: &'a Store,
    pub session: &'a Session,
    pub config: &'a model::Config,
    pub project: String,
    /// The working tree the citations are checked against.
    pub folder: Option<PathBuf>,
    /// A configured profile's name (default: the answer model).
    pub model: Option<String>,
    pub budget: Option<usize>,
    /// The pack's layout (`standard` or `cache`) and the cache layout's
    /// prefix share; unset, [`default_layout`].
    pub layout: Option<String>,
    pub prefix_share: Option<f64>,
    pub json: bool,
}

/// The layout of a long-context call's pack when the call names none.
pub fn default_layout(profile: &model::Profile) -> &'static str {
    let _ = profile;
    "standard"
}

/// A pack's prompt cache key: `graphicle:<project>@<commit12>:<prefix12>`,
/// from the project's name, its commit and the pack's prefix, with nothing
/// the question says.
pub fn cache_key(pack: &Value) -> String {
    let project: String = pack["project"]
        .as_str()
        .unwrap_or("project")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(24)
        .collect();
    let commit: String = pack["commit"]
        .as_str()
        .unwrap_or("nocommit")
        .chars()
        .take(12)
        .collect();
    let prefix: String = pack["prefix_sha256"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(12)
        .collect();
    format!("graphicle:{project}@{commit}:{prefix}")
}

/// The configured profile `name` (else the answer model).
pub(crate) fn profile(run: &Run<'_>) -> Result<(String, model::Profile)> {
    let configuration =
        crate::settings::configuration(run.store, &run.session.workspace, run.config)?;
    let (name, profile) = configuration.profile(run.model.as_deref()).map_err(|_| {
        Error::bad(match &run.model {
            Some(name) => format!("No model profile named {name}: graphicle config show lists them"),
            None => "No answer model: choose one with `graphicle config set model meta` (Muse Spark) or `graphicle config set model meta:muse-wonder-1.0` (Muse Wonder), or another provider".into(),
        })
    })?;
    if profile.protocol == "local_onnx" {
        return Err(Error::bad(
            "A local ONNX profile embeds or reranks; choose a chat model",
        ));
    }
    Ok((name.to_string(), profile.clone()))
}

/// The pack's budget: `asked`, else the profile's input limit less
/// [`RESERVE`]; never more than the profile takes.
pub fn budget_for(profile: &model::Profile, asked: Option<usize>) -> Result<usize> {
    budget_with(profile, asked, None)
}

/// [`budget_for`] with a profile's calibration: when the provider counted
/// `ratio` of Graphicle's estimate on earlier calls, a pack without
/// `--budget` is sized so that the provider's count fills the room less
/// [`CALIBRATION_MARGIN`].
pub fn budget_with(
    profile: &model::Profile,
    asked: Option<usize>,
    ratio: Option<f64>,
) -> Result<usize> {
    let room = profile.input_limit.saturating_sub(RESERVE);
    let calibrated = ratio
        .filter(|r| r.is_finite() && *r > 0.0)
        .map(|r| ((room as f64) * (1.0 - CALIBRATION_MARGIN) / r) as usize);
    if asked.is_none()
        && let Some(budget) = calibrated
    {
        return Ok(budget.clamp(crate::code_pack::MIN_BUDGET, crate::code_pack::MAX_BUDGET));
    }
    let budget = asked.unwrap_or(room).min(crate::code_pack::MAX_BUDGET);
    if budget > room {
        return Err(Error::bad(format!(
            "--budget {budget} does not fit the model's input limit of {} tokens with room for the question ({} tokens at most)",
            profile.input_limit, room
        )));
    }
    if budget < crate::code_pack::MIN_BUDGET {
        return Err(Error::bad(format!(
            "The model's input limit ({} tokens) leaves no room for a pack: raise it with `graphicle config set models.NAME.input_limit ...`",
            profile.input_limit
        )));
    }
    Ok(budget)
}

/// The share of the room a calibrated pack leaves free.
pub const CALIBRATION_MARGIN: f64 = 0.03;
/// The file in the data folder that keeps each profile's calibration.
const CALIBRATION_FILE: &str = "pack-calibration.json";

/// A profile's calibration: the provider's prompt tokens over Graphicle's
/// estimate of the same prompt, from earlier `ask --pack` and `plan` calls
/// of the same model.
pub fn calibration(store: &Store, name: &str, profile: &model::Profile) -> Option<f64> {
    let text = std::fs::read_to_string(store.directory.join(CALIBRATION_FILE)).ok()?;
    let all: Value = serde_json::from_str(&text).ok()?;
    let entry = &all[name];
    (entry["model"] == profile.model.as_str())
        .then(|| entry["ratio"].as_f64())
        .flatten()
        .filter(|r| (0.3..=2.0).contains(r))
}

/// Record a call's ratio of the provider's prompt tokens to the estimate,
/// averaged with the earlier one.
pub fn record_calibration(
    store: &Store,
    name: &str,
    profile: &model::Profile,
    prompt_tokens: u64,
    estimated: usize,
) {
    if estimated == 0 || prompt_tokens == 0 {
        return;
    }
    let ratio = (prompt_tokens as f64 / estimated as f64).clamp(0.3, 2.0);
    let path = store.directory.join(CALIBRATION_FILE);
    let mut all: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let ratio = match calibration(store, name, profile) {
        Some(earlier) => (earlier + ratio) / 2.0,
        None => ratio,
    };
    let calls = all[name]["calls"].as_u64().unwrap_or(0) + 1;
    all[name] =
        json!({"model":profile.model,"ratio":(ratio * 10_000.0).round() / 10_000.0,"calls":calls});
    let _ = std::fs::write(&path, format!("{all:#}\n"));
}

/// Graphicle's estimate of a request: the pack's tokens, and the
/// instructions and the rest of the request (the question, or what `fix`
/// adds after the pack) at the estimator's default rate.
pub(crate) fn request_estimate(pack: &Value, instructions: &str, rest: &str) -> u64 {
    pack["tokens"].as_u64().unwrap_or(0)
        + crate::code_pack::estimate::tokens(
            instructions.len() + rest.len() + 32,
            crate::code_pack::estimate::PACK_TEXT_BYTES_PER_TOKEN,
        ) as u64
}

/// [`request_estimate`] for a pack sized by the estimator (the requests whose
/// provider counts calibrate the profile).
fn prompt_estimate(pack: &Value, instructions: &str, question: &str) -> Option<usize> {
    (pack["tokenizer"] == "estimate")
        .then(|| request_estimate(pack, instructions, question) as usize)
}

/// A `path:line` or `path:start-end` an answer cites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
    pub line: usize,
    pub end: usize,
}

static CITATION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"([A-Za-z0-9_][A-Za-z0-9_.\-/]*\.[A-Za-z0-9]+):(\d+)(?:-(\d+))?").unwrap()
});

/// The citations in `text`, each once, in order.
pub fn citations(text: &str) -> Vec<Citation> {
    let mut found: Vec<Citation> = Vec::new();
    for capture in CITATION.captures_iter(text) {
        let path = capture[1].trim_start_matches("./").to_string();
        let Ok(line) = capture[2].parse::<usize>() else {
            continue;
        };
        let end = capture
            .get(3)
            .and_then(|m| m.as_str().parse::<usize>().ok())
            .unwrap_or(line);
        let citation = Citation { path, line, end };
        if !found.contains(&citation) {
            found.push(citation);
        }
    }
    found
}

/// Line counts of the working tree's files, read once each.
pub struct Tree {
    folder: Option<PathBuf>,
    lines: HashMap<String, Option<usize>>,
}

impl Tree {
    pub fn new(folder: Option<PathBuf>) -> Self {
        Self {
            folder,
            lines: HashMap::new(),
        }
    }

    /// The number of lines of `path`, if it is a file of the tree.
    pub fn lines(&mut self, path: &str) -> Option<usize> {
        if let Some(hit) = self.lines.get(path) {
            return *hit;
        }
        let count = self.folder.as_ref().and_then(|folder| {
            let relative = Path::new(path);
            if relative.is_absolute() || path.split('/').any(|part| part == "..") {
                return None;
            }
            let text = std::fs::read(folder.join(relative)).ok()?;
            Some(String::from_utf8_lossy(&text).lines().count())
        });
        self.lines.insert(path.to_string(), count);
        count
    }

    pub fn exists(&mut self, path: &str) -> bool {
        self.lines(path).is_some()
    }

    /// Why `citation` does not hold in the tree, if it does not.
    pub fn problem(&mut self, path: &str, line: usize, end: usize) -> Option<String> {
        if self.folder.is_none() {
            return Some("no working tree to check against".into());
        }
        let Some(lines) = self.lines(path) else {
            return Some("no such file in the working tree".into());
        };
        if line == 0 || end < line {
            return Some(format!("{line}-{end} is not a line range"));
        }
        if end > lines {
            return Some(format!("past the end of the file ({lines} lines)"));
        }
        None
    }
}

pub(crate) fn usage_json(done: &model::Completion, price: Option<&model::Price>) -> Value {
    let cost = match (price, done.input_tokens, done.output_tokens) {
        (Some(price), Some(input), Some(output)) => {
            Some(price.cost(input, done.cached_tokens.unwrap_or(0), output))
        }
        _ => None,
    };
    json!({"input_tokens":done.input_tokens,"output_tokens":done.output_tokens,
        "cached_tokens":done.cached_tokens,"reasoning_tokens":done.reasoning_tokens,
        "seconds":done.elapsed_ms as f64 / 1000.0,"cost_usd":cost})
}

/// Append a call to the spend log ([`crate::spend`]), releasing its
/// reservation, and return its entry; a log that cannot be written is
/// reported, and the answer still printed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_call(
    run: &Run<'_>,
    command: &str,
    name: &str,
    profile: &model::Profile,
    pack: &Value,
    key: &str,
    result: &Result<model::Completion>,
    held: &crate::spend::Reservation,
) -> Value {
    let call = crate::spend::Call {
        command,
        profile: name,
        settings: profile,
        project: pack["project"].as_str(),
        commit: pack["commit"].as_str(),
        prompt_cache_key: model::prompt_cache_on(profile).then_some(key),
    };
    let mut entry = crate::spend::entry(&call, result, chrono::Utc::now());
    if let Err(error) = crate::spend::settle(&run.store.directory, held, &mut entry) {
        eprintln!("graphicle: could not write the spend log: {error}");
    }
    entry
}

/// One long-context request: the question its pack is made for, the text
/// that goes after the pack (the question, or what `fix` adds), the
/// instructions, how the request's text is made from a pack, and the
/// schema of a structured reply.
pub(crate) struct Request<'a> {
    pub question: &'a str,
    pub rest: &'a str,
    pub instructions: &'a str,
    pub user_of: &'a (dyn Fn(&Value) -> String + Sync),
    pub schema: Option<&'a Value>,
}

/// Send `request` with `pack`: the caps reserve its worst case first
/// ([`crate::spend::reserve`]: the provider's count, else the estimate
/// with the calibration `ratio` and a margin), the spend log records it, and
/// the profile's calibration takes the provider's count of an estimated
/// pack. The call's result, and its log entry; an error only when a cap
/// refuses the call.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send(
    run: &Run<'_>,
    command: &str,
    name: &str,
    profile: &model::Profile,
    pack: &Value,
    request: &Request<'_>,
    counted: Option<u64>,
    ratio: Option<f64>,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> Result<(Result<model::Completion>, Value)> {
    let key = cache_key(pack);
    let estimate = request_estimate(pack, request.instructions, request.rest);
    let held = crate::spend::reserve(
        &run.store.directory,
        name,
        profile,
        crate::spend::worst_input(estimate, counted, ratio),
        chrono::Utc::now(),
    )?;
    let user = (request.user_of)(pack);
    let result = model::complete(
        profile,
        request.instructions,
        &user,
        request.schema,
        profile.output_limit,
        Some(&key),
        on_text,
    )
    .await;
    let entry = log_call(run, command, name, profile, pack, &key, &result, &held);
    if let Ok(done) = &result
        && let Some(prompt) = done.input_tokens
        && let Some(estimated) = prompt_estimate(pack, request.instructions, request.rest)
    {
        record_calibration(run.store, name, profile, prompt, estimated);
    }
    Ok((result, entry))
}

/// The pack of `question` at `budget`; for the cache layout, its repository
/// part sized from `nominal` when given ([`crate::code_pack::Options`]'s
/// `prefix_budget`).
fn pack_for(
    run: &Run<'_>,
    profile: &model::Profile,
    question: &str,
    budget: usize,
    nominal: Option<usize>,
) -> Result<Value> {
    let args = pack_args(run, profile, question, budget, nominal);
    run.store
        .project_pack(&run.session.workspace, &run.session.capability, &args)
}

/// [`pack_for`]'s arguments to [`crate::store::Store::project_pack`], for a
/// caller that adds its own (`review`'s pinned focus).
pub(crate) fn pack_args(
    run: &Run<'_>,
    profile: &model::Profile,
    question: &str,
    budget: usize,
    nominal: Option<usize>,
) -> Value {
    let layout = match (&run.layout, run.prefix_share) {
        (Some(layout), _) => layout.as_str(),
        (None, Some(_)) => "cache",
        (None, None) => default_layout(profile),
    };
    let mut args = json!({"repository_id":run.project,"question":question,"budget":budget,
        "layout":layout});
    if let Some(share) = run.prefix_share {
        args["prefix_share"] = json!(share);
    }
    if let Some(nominal) = nominal {
        args["prefix_budget"] = json!(nominal);
    }
    if let Some(tokenizer) = &profile.tokenizer {
        args["tokenizer"] = json!(tokenizer);
    }
    args
}

/// A request is counted by the provider ([`model::count_tokens_on`]) when
/// its estimate comes to this share of the input limit.
const COUNT_FROM: f64 = 0.8;
/// Packs made again at most while a request does not fit.
const REFITS: usize = 3;

/// The pack for `request` at `budget`, fitted to the input limit ([`fit`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fitted_pack(
    run: &Run<'_>,
    profile: &model::Profile,
    request: &Request<'_>,
    budget: usize,
    nominal: Option<usize>,
    ratio: Option<f64>,
    human: bool,
) -> Result<(Value, usize, Option<u64>)> {
    let pack = pack_for(run, profile, request.question, budget, nominal)?;
    fit(run, profile, request, pack, budget, nominal, ratio, human).await
}

/// `pack` (made at `budget`) fitted to the input limit with `request`: the
/// request's estimate times the profile's calibration `ratio` must fit, and
/// where the profile counts ([`model::count_tokens_on`]) a request near the
/// limit is counted with the provider. While it would not fit, the pack is
/// made again smaller by what is over. The pack, the budget it was made at,
/// and the provider's count when there is one (else the estimate stands, as
/// it does when counting fails).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fit(
    run: &Run<'_>,
    profile: &model::Profile,
    request: &Request<'_>,
    pack: Value,
    budget: usize,
    nominal: Option<usize>,
    ratio: Option<f64>,
    human: bool,
) -> Result<(Value, usize, Option<u64>)> {
    let repack = |budget: usize| pack_for(run, profile, request.question, budget, nominal);
    fit_with(profile, request, pack, budget, ratio, human, &repack).await
}

/// [`fit`], packing again with `repack` (a pack at the budget it is given).
pub(crate) async fn fit_with(
    profile: &model::Profile,
    request: &Request<'_>,
    mut pack: Value,
    mut budget: usize,
    ratio: Option<f64>,
    human: bool,
    repack: &(dyn Fn(usize) -> Result<Value> + Sync),
) -> Result<(Value, usize, Option<u64>)> {
    let limit = profile.input_limit;
    let counts = model::count_tokens_on(profile);
    let ratio = ratio.filter(|r| r.is_finite() && *r > 0.0).unwrap_or(1.0);
    for refit in 0..=REFITS {
        let estimate = request_estimate(&pack, request.instructions, request.rest) as f64;
        let predicted = (estimate * ratio).ceil() as usize;
        let (over, by) = if counts && (refit > 0 || predicted as f64 >= COUNT_FROM * limit as f64) {
            match model::count_input_tokens(
                profile,
                request.instructions,
                &(request.user_of)(&pack),
                request.schema,
            )
            .await
            {
                Ok(counted) if counted as usize <= limit => {
                    return Ok((pack, budget, Some(counted)));
                }
                Ok(counted) => (counted as usize, "the provider counted"),
                Err(error) => {
                    if human {
                        eprintln!(
                            "graphicle: the provider did not count the request ({error}); sizing it by the estimate"
                        );
                    }
                    if predicted <= limit {
                        return Ok((pack, budget, None));
                    }
                    (predicted, "the estimate comes to")
                }
            }
        } else if predicted <= limit {
            return Ok((pack, budget, None));
        } else {
            (predicted, "the estimate comes to")
        };
        // The pack's part of the request, shrunk by what is over and a margin,
        // in the estimate's tokens.
        let observed = over as f64 / estimate.max(1.0);
        let excess = ((over - limit) as f64 + CALIBRATION_MARGIN * limit as f64) / observed;
        let smaller = (pack["tokens"].as_f64().unwrap_or(0.0) - excess).floor();
        let smaller = if smaller > 0.0 { smaller as usize } else { 0 };
        if refit == REFITS || smaller < crate::code_pack::MIN_BUDGET || smaller >= budget {
            return Err(Error::bad(format!(
                "For this request {by} {over} tokens, over the model's input limit of {limit}, and a smaller pack leaves no room: ask with a smaller --budget or a shorter text"
            )));
        }
        if human {
            eprintln!(
                "graphicle: for this request {by} {} tokens, over the input limit of {}; packing again at {} tokens",
                crate::terminal::count(over as u64),
                crate::terminal::count(limit as u64),
                crate::terminal::count(smaller as u64)
            );
        }
        budget = smaller;
        pack = repack(budget)?;
    }
    Ok((pack, budget, None))
}

pub(crate) fn tell_packed(pack: &Value, name: &str, profile: &model::Profile) {
    let layout = if pack["layout"] == "cache" {
        format!(
            ", {} in the repository part",
            crate::terminal::count(pack["prefix_tokens"].as_u64().unwrap_or(0))
        )
    } else {
        String::new()
    };
    eprintln!(
        "graphicle: packed {} tokens of {} files ({}{layout}) in {} s; asking {name} ({})...",
        crate::terminal::count(pack["tokens"].as_u64().unwrap_or(0)),
        pack["included"].as_array().map_or(0, Vec::len),
        pack["tokenizer"].as_str().unwrap_or("estimate"),
        pack["seconds"],
        profile.model
    );
}

/// `graphicle ask --pack`.
pub async fn ask(run: Run<'_>, question: &str) -> Result<()> {
    let (name, profile) = profile(&run)?;
    let ratio = calibration(run.store, &name, &profile);
    let budget = budget_with(&profile, run.budget, ratio)?;
    // The repository part of a cache-layout pack is sized from the budget
    // before calibration, so that it stays the same bytes from call to call.
    let nominal = budget_for(&profile, run.budget)?;
    let human = crate::terminal::human(run.json);
    // The stable instructions, then the pack (its repository part first),
    // then the question.
    let user_of =
        |pack: &Value| format!("{}\nQuestion: {question}\n", crate::code_pack::text(pack));
    let request = Request {
        question,
        rest: question,
        instructions: ASK_INSTRUCTIONS,
        user_of: &user_of,
        schema: None,
    };
    let (pack, _, counted) = fitted_pack(
        &run,
        &profile,
        &request,
        budget,
        Some(nominal),
        ratio,
        human,
    )
    .await?;
    if human {
        tell_packed(&pack, &name, &profile);
    }
    let key = cache_key(&pack);
    let mut stdout = std::io::stdout();
    let mut show = |delta: &str| {
        if human {
            let _ = stdout.write_all(delta.as_bytes());
            let _ = stdout.flush();
        }
    };
    let (result, _) = send(
        &run,
        "ask --pack",
        &name,
        &profile,
        &pack,
        &request,
        counted,
        ratio,
        &mut show,
    )
    .await?;
    let done = result?;
    let mut tree = Tree::new(run.folder.clone());
    let checked: Vec<Value> = citations(&done.text)
        .into_iter()
        .map(|c| {
            let problem = tree.problem(&c.path, c.line, c.end);
            json!({"path":c.path,"line":c.line,"end_line":c.end,"valid":problem.is_none(),"problem":problem})
        })
        .collect();
    let usage = usage_json(&done, profile.price.as_ref());
    if !human {
        let output = json!({"answer":done.text,"model":done.model,"profile":name,
            "citations":checked,"invalid_citations":checked.iter().filter(|c| c["valid"] != true).count(),
            "usage":usage,"prompt_cache_key":model::prompt_cache_on(&profile).then_some(&key),
            "input_tokens_counted":counted,"pack":crate::code_pack::summary(&pack)});
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let mut tail = String::from("\n\n");
    let valid: Vec<&Value> = checked.iter().filter(|c| c["valid"] == true).collect();
    let invalid: Vec<&Value> = checked.iter().filter(|c| c["valid"] != true).collect();
    let cite = |c: &Value| {
        if c["line"] == c["end_line"] {
            format!("{}:{}", c["path"].as_str().unwrap_or(""), c["line"])
        } else {
            format!(
                "{}:{}-{}",
                c["path"].as_str().unwrap_or(""),
                c["line"],
                c["end_line"]
            )
        }
    };
    if !valid.is_empty() {
        tail.push_str("Sources (checked against the working tree):\n");
        for c in &valid {
            tail.push_str(&format!("  {}\n", cite(c)));
        }
    }
    if !invalid.is_empty() {
        tail.push_str("Citations that do not hold in the working tree:\n");
        for c in &invalid {
            tail.push_str(&format!(
                "  {}: {}\n",
                cite(c),
                c["problem"].as_str().unwrap_or("")
            ));
        }
    }
    if checked.is_empty() {
        tail.push_str("The answer cites no path:line.\n");
    }
    tail.push_str(&crate::spend::line(&usage));
    tail.push('\n');
    print!("{tail}");
    Ok(())
}

/// The JSON object in a model's text: the text itself, or the part between
/// its first `{` and last `}` (fences and prose around it are dropped).
pub(crate) fn json_in(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Ok(value);
    }
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err(Error::bad("The model's plan is not JSON"));
    };
    serde_json::from_str(&text[start..=end]).map_err(|_| Error::bad("The model's plan is not JSON"))
}

/// The plan with each file checked against the working tree: edits must be
/// existing files with ranges inside them, new files must not exist yet,
/// tests to update or run must exist.
pub fn check_plan(plan: &mut Value, tree: &mut Tree) -> usize {
    let mut problems = 0;
    if let Some(edits) = plan["edits"].as_array_mut() {
        for edit in edits {
            let path = edit["path"].as_str().unwrap_or("").to_string();
            let start = edit["start_line"].as_u64().unwrap_or(0) as usize;
            let end = edit["end_line"].as_u64().unwrap_or(0) as usize;
            let problem = tree.problem(&path, start, end);
            problems += usize::from(problem.is_some());
            edit["valid"] = json!(problem.is_none());
            edit["problem"] = json!(problem);
        }
    }
    if let Some(files) = plan["new_files"].as_array_mut() {
        for file in files {
            let path = file["path"].as_str().unwrap_or("").to_string();
            let exists = tree.exists(&path);
            problems += usize::from(exists);
            file["valid"] = json!(!exists);
            file["problem"] = json!(exists.then_some("the file exists already"));
        }
    }
    if let Some(tests) = plan["tests"].as_array_mut() {
        for test in tests {
            let path = test["path"].as_str().unwrap_or("").to_string();
            let needs = test["action"] != "add";
            let missing = needs && !path.is_empty() && !tree.exists(&path);
            problems += usize::from(missing);
            test["valid"] = json!(!missing);
            test["problem"] = json!(missing.then_some("no such file in the working tree"));
        }
    }
    problems
}

/// The plan as Markdown.
pub fn plan_markdown(request: &str, plan: &Value) -> String {
    let mark = |item: &Value| match item["problem"].as_str() {
        Some(problem) => format!(" (check: {problem})"),
        None => String::new(),
    };
    let mut out = format!("# Plan: {}\n\n", request.lines().next().unwrap_or(request));
    if let Some(summary) = plan["summary"].as_str().filter(|s| !s.is_empty()) {
        out.push_str(&format!("{summary}\n\n"));
    }
    let list = |key: &str| plan[key].as_array().cloned().unwrap_or_default();
    let steps = list("steps");
    if !steps.is_empty() {
        out.push_str("## Steps\n\n");
        for (i, step) in steps.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, step.as_str().unwrap_or("")));
        }
        out.push('\n');
    }
    let edits = list("edits");
    if !edits.is_empty() {
        out.push_str("## Files to edit\n\n");
        for edit in &edits {
            out.push_str(&format!(
                "- {}:{}-{}: {}{}\n",
                edit["path"].as_str().unwrap_or(""),
                edit["start_line"],
                edit["end_line"],
                edit["reason"].as_str().unwrap_or(""),
                mark(edit)
            ));
        }
        out.push('\n');
    }
    let new_files = list("new_files");
    if !new_files.is_empty() {
        out.push_str("## New files\n\n");
        for file in &new_files {
            out.push_str(&format!(
                "- {}: {}{}\n",
                file["path"].as_str().unwrap_or(""),
                file["purpose"].as_str().unwrap_or(""),
                mark(file)
            ));
        }
        out.push('\n');
    }
    let tests = list("tests");
    if !tests.is_empty() {
        out.push_str("## Tests\n\n");
        for test in &tests {
            out.push_str(&format!(
                "- {} {}: {}{}\n",
                test["action"].as_str().unwrap_or("run"),
                test["path"].as_str().unwrap_or(""),
                test["what"].as_str().unwrap_or(""),
                mark(test)
            ));
        }
        out.push('\n');
    }
    let risks = list("risks");
    if !risks.is_empty() {
        out.push_str("## Risks\n\n");
        for risk in &risks {
            out.push_str(&format!("- {}\n", risk.as_str().unwrap_or("")));
        }
        out.push('\n');
    }
    out
}

/// `graphicle plan`.
pub async fn plan(run: Run<'_>, request: &str) -> Result<()> {
    let (name, profile) = profile(&run)?;
    let ratio = calibration(run.store, &name, &profile);
    let budget = budget_with(&profile, run.budget, ratio)?;
    let nominal = budget_for(&profile, run.budget)?;
    let human = crate::terminal::human(run.json);
    let schema = plan_schema();
    let user_of = |pack: &Value| {
        format!(
            "{}\nChange request: {request}\n",
            crate::code_pack::text(pack)
        )
    };
    let call = Request {
        question: request,
        rest: request,
        instructions: PLAN_INSTRUCTIONS,
        user_of: &user_of,
        schema: Some(&schema),
    };
    let (pack, _, counted) =
        fitted_pack(&run, &profile, &call, budget, Some(nominal), ratio, human).await?;
    if human {
        tell_packed(&pack, &name, &profile);
    }
    let key = cache_key(&pack);
    let (result, _) = send(
        &run,
        "plan",
        &name,
        &profile,
        &pack,
        &call,
        counted,
        ratio,
        &mut |_| {},
    )
    .await?;
    let done = result?;
    let mut plan = json_in(&done.text)?;
    if !plan.is_object() {
        return Err(Error::bad("The model's plan is not a JSON object"));
    }
    let mut tree = Tree::new(run.folder.clone());
    let problems = check_plan(&mut plan, &mut tree);
    let usage = usage_json(&done, profile.price.as_ref());
    if !human {
        let output = json!({"request":request,"plan":plan,"problems":problems,"model":done.model,
            "profile":name,"usage":usage,"prompt_cache_key":model::prompt_cache_on(&profile).then_some(&key),
            "input_tokens_counted":counted,"pack":crate::code_pack::summary(&pack)});
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    let mut text = plan_markdown(request, &plan);
    if problems > 0 {
        text.push_str(&format!(
            "{problems} item(s) of the plan do not hold in the working tree (marked check).\n"
        ));
    }
    text.push_str(&crate::spend::line(&usage));
    text.push('\n');
    print!("{text}");
    Ok(())
}

/// Lines per file of a pack's citations, for tests.
#[cfg(test)]
fn by_path(cites: &[Citation]) -> std::collections::BTreeMap<String, Vec<usize>> {
    let mut out: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for c in cites {
        out.entry(c.path.clone()).or_default().push(c.line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_are_found_once_each_in_order() {
        let text = "The check is in `pkg/a/x.go:42` and pkg/a/x.go:50-60; see also ./cmd/main.go:7, \
                    pkg/a/x.go:42 again, and version 1.2:3 is not a file. README.md:1";
        let found = citations(text);
        assert_eq!(
            found,
            vec![
                Citation {
                    path: "pkg/a/x.go".into(),
                    line: 42,
                    end: 42
                },
                Citation {
                    path: "pkg/a/x.go".into(),
                    line: 50,
                    end: 60
                },
                Citation {
                    path: "cmd/main.go".into(),
                    line: 7,
                    end: 7
                },
                Citation {
                    path: "1.2".into(),
                    line: 3,
                    end: 3
                },
                Citation {
                    path: "README.md".into(),
                    line: 1,
                    end: 1
                },
            ]
        );
        assert_eq!(by_path(&found)["pkg/a/x.go"], vec![42, 50]);
    }

    fn tree() -> (PathBuf, Tree) {
        let dir = std::env::temp_dir().join(format!(
            "graphicle-lc-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/x.go"), "package x\n\nfunc A() {}\n").unwrap();
        (dir.clone(), Tree::new(Some(dir)))
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn citations_are_checked_against_the_tree() {
        let (dir, mut tree) = tree();
        assert_eq!(tree.problem("pkg/x.go", 3, 3), None);
        assert_eq!(tree.problem("pkg/x.go", 1, 3), None);
        assert!(tree.problem("pkg/x.go", 4, 4).unwrap().contains("3 lines"));
        assert!(
            tree.problem("pkg/y.go", 1, 1)
                .unwrap()
                .contains("no such file")
        );
        assert!(tree.problem("../etc/passwd", 1, 1).is_some());
        assert!(tree.problem("pkg/x.go", 3, 2).is_some());
        assert!(Tree::new(None).problem("pkg/x.go", 1, 1).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_plan_is_checked_and_rendered() {
        let (dir, mut tree) = tree();
        let mut plan = json!({"summary":"Add B.","steps":["Write B","Test B"],
            "edits":[{"path":"pkg/x.go","start_line":3,"end_line":3,"reason":"call B"},
                     {"path":"pkg/x.go","start_line":2,"end_line":9,"reason":"too far"},
                     {"path":"pkg/z.go","start_line":1,"end_line":1,"reason":"missing"}],
            "new_files":[{"path":"pkg/b.go","purpose":"B"},{"path":"pkg/x.go","purpose":"exists"}],
            "tests":[{"action":"add","path":"pkg/b_test.go","what":"B works"},
                     {"action":"run","path":"pkg/x_test.go","what":"nothing there"}],
            "risks":["B is slow"]});
        assert_eq!(check_plan(&mut plan, &mut tree), 4);
        assert_eq!(plan["edits"][0]["valid"], true);
        assert_eq!(plan["edits"][1]["valid"], false);
        assert_eq!(plan["new_files"][0]["valid"], true);
        assert_eq!(plan["new_files"][1]["valid"], false);
        assert_eq!(plan["tests"][0]["valid"], true);
        assert_eq!(plan["tests"][1]["valid"], false);
        let text = plan_markdown("Add B", &plan);
        assert!(text.starts_with("# Plan: Add B\n\nAdd B.\n\n## Steps\n\n1. Write B\n2. Test B\n"));
        assert!(text.contains("- pkg/x.go:3-3: call B\n"));
        assert!(
            text.contains("- pkg/x.go:2-9: too far (check: past the end of the file (3 lines))\n")
        );
        assert!(text.contains("## Risks\n\n- B is slow\n"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plans_are_read_from_fenced_or_bare_json() {
        assert_eq!(json_in("{\"a\":1}").unwrap()["a"], 1);
        assert_eq!(json_in("Here:\n```json\n{\"a\":2}\n```\n").unwrap()["a"], 2);
        assert!(json_in("no plan").is_err());
    }

    #[test]
    fn the_budget_fits_the_profile() {
        let profile: model::Profile = serde_json::from_value(json!({"protocol":"chat",
            "base_url":"https://api.meta.ai/v1","model":"muse-spark-1.3","key_env":"MODEL_API_KEY",
            "input_limit":1_016_576,"output_limit":32_000}))
        .unwrap();
        assert_eq!(budget_for(&profile, None).unwrap(), 1_016_576 - RESERVE);
        assert_eq!(budget_for(&profile, Some(128_000)).unwrap(), 128_000);
        assert!(budget_for(&profile, Some(1_100_000)).is_err());
        let small: model::Profile = serde_json::from_value(json!({"protocol":"chat",
            "base_url":"https://api.meta.ai/v1","model":"m","input_limit":4_000}))
        .unwrap();
        assert!(budget_for(&small, None).is_err());
    }

    #[test]
    fn a_calibrated_profile_fills_its_room() {
        let profile: model::Profile = serde_json::from_value(json!({"protocol":"chat",
            "base_url":"https://api.meta.ai/v1","model":"muse-spark-1.3","input_limit":104_000}))
        .unwrap();
        let room = 100_000.0;
        // The provider counted 0.8 of the estimate: a larger pack by estimate.
        let budget = budget_with(&profile, None, Some(0.8)).unwrap();
        assert_eq!(budget, (room * (1.0 - CALIBRATION_MARGIN) / 0.8) as usize);
        // --budget wins over the calibration.
        assert_eq!(
            budget_with(&profile, Some(50_000), Some(0.8)).unwrap(),
            50_000
        );
        assert_eq!(budget_with(&profile, None, None).unwrap(), 100_000);
        let pack = json!({"tokenizer":"estimate","tokens":1000});
        assert!(prompt_estimate(&pack, "abc", "def").unwrap() > 1000);
        assert!(prompt_estimate(&json!({"tokenizer":"x","tokens":1}), "a", "b").is_none());
    }

    #[test]
    fn costs_come_from_the_configured_price() {
        let price = model::Price {
            input: 1.25,
            output: 4.25,
            cached_input: Some(0.15),
            per_image: None,
        };
        let cost = price.cost(1_000_000, 200_000, 10_000);
        assert!((cost - (0.8 * 1.25 + 0.2 * 0.15 + 0.01 * 4.25)).abs() < 1e-9);
        let done = model::Completion {
            input_tokens: Some(1000),
            output_tokens: Some(100),
            ..Default::default()
        };
        assert!(usage_json(&done, None)["cost_usd"].is_null());
        assert!(
            usage_json(&done, Some(&price))["cost_usd"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        let line = crate::spend::line(&usage_json(&done, Some(&price)));
        assert!(line.starts_with("1,000 tokens in, 100 out; $"), "{line}");
    }
    #[test]
    fn the_cache_key_names_the_project_commit_and_prefix_only() {
        let pack = json!({"project":"my repo/\u{fc}","commit":"0123456789abcdef",
            "prefix_sha256":"e3b0c44298fc1c149afbf4c8996fb924","question":"a secret question"});
        let key = cache_key(&pack);
        assert_eq!(key, "graphicle:my-repo--@0123456789ab:e3b0c44298fc");
        assert!(!key.contains("secret"));
        assert_eq!(cache_key(&json!({"project":"p"})), "graphicle:p@nocommit:");
    }
}
