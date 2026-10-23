//! Claude Code's prompt-context hook (`graphicle setup --prompt-context`,
//! off by default): on UserPromptSubmit, `graphicle hook claude-code
//! prompt-context` runs `graphicle context` on the prompt and hands Claude
//! Code the packet's Markdown as `additionalContext`, so the agent starts
//! with the retrieval's excerpts. `graphicle context` keeps a packet within
//! [`crate::query::PUSH_CHARS`] (8,400) characters, below the 10,000 that
//! Claude Code shows of a hook's output (it shows a longer one as a 2 KB
//! preview).
//!
//! A large project whose passage index is not ready ([`crate::passage_index`]:
//! not built yet, or many files changed since it was) gets the quick packet
//! instead: the files that define the code names the prompt gives, from the
//! code graph's symbols, ranked by those names (`context`'s `graph_only`),
//! without reading every file. A small project, and a project with reviewed
//! memory (whose memories only the full path reads), are asked in full
//! ([`crate::indexed_query::hook_route`]).
//!
//! The hook adds no context, and never holds the prompt up for long: when the
//! prompt has fewer than [`MIN_CHARS`] characters besides spaces, when it is
//! not a code question ([`crate::code_question::is_code_question`], the
//! classifier `context` itself uses), when the folder is in no Graphicle
//! project, when `graphicle context` fails or prints no Markdown, and when
//! it has not answered within the budget ([`budget`]: `prompt_context.seconds`,
//! [`SECONDS`] unless set, counted from the hook's start; it is then
//! stopped). When it gives up for time, or answers from the quick packet, it
//! says so to the person once per session, as Claude Code's `systemMessage`
//! ([`note_once`], [`note_for`]).
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Characters a prompt needs, spaces left out, to get context: shorter
/// ones ("yes", "go on", "run the tests") are replies, not questions.
pub const MIN_CHARS: usize = 16;

/// Seconds the hook waits for `graphicle context` before it gives up and
/// adds nothing, unless `prompt_context.seconds` or
/// [`SECONDS_VARIABLE`] says otherwise ([`budget`]).
pub const SECONDS: u64 = 10;
/// Most seconds `prompt_context.seconds` may give.
pub const MAX_SECONDS: u64 = 25;
/// The environment variable that sets the budget, over the configuration.
pub const SECONDS_VARIABLE: &str = "GRAPHICLE_PROMPT_CONTEXT_SECONDS";

/// The timeout of the hook's entry in Claude Code's settings, in seconds:
/// a backstop above [`MAX_SECONDS`], so Claude Code never waits longer.
pub const HOOK_TIMEOUT: u64 = 30;
const _: () = assert!(HOOK_TIMEOUT > MAX_SECONDS);

/// The hook's budget: [`SECONDS_VARIABLE`] when it holds 1 to
/// [`MAX_SECONDS`], else the configuration's `prompt_context.seconds`, else
/// [`SECONDS`].
pub fn budget(config: Option<&crate::model::Config>) -> Duration {
    let from_environment = std::env::var(SECONDS_VARIABLE)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| (1..=MAX_SECONDS).contains(seconds));
    let seconds = from_environment
        .or_else(|| {
            config
                .and_then(|config| config.prompt_context.as_ref())
                .and_then(|settings| settings.seconds)
        })
        .unwrap_or(SECONDS);
    Duration::from_secs(seconds.clamp(1, MAX_SECONDS))
}

/// The question the hook asks for `payload` (Claude Code's UserPromptSubmit
/// input): its `prompt`, when that is long enough and a code question,
/// shortened as `context` would shorten it past 16,000 bytes; `None`
/// otherwise.
pub fn question(payload: &Value) -> Option<String> {
    let prompt = payload["prompt"].as_str()?.trim();
    if prompt.chars().filter(|c| !c.is_whitespace()).count() < MIN_CHARS
        || !crate::code_question::is_code_question(prompt)
    {
        return None;
    }
    Some(
        crate::code_question::shorten(prompt)
            .map_or_else(|| prompt.to_string(), |(shortened, _)| shortened),
    )
}

/// Claude Code's output for `markdown`: the packet as the prompt's
/// additional context.
pub fn output(markdown: &str) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":markdown}})
}

/// Run the hook for the data folder `directory` on `payload`: print
/// [`output`] when the prompt gets context, else nothing, with a note for the
/// person when it gave up for time or answered from the quick packet.
pub fn run(directory: &Path, payload: &Value) {
    use std::io::Write;
    let started = Instant::now();
    let Some(question) = question(payload) else {
        return;
    };
    let Some(cwd) = payload["cwd"]
        .as_str()
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_dir())
        .or_else(|| std::env::current_dir().ok())
    else {
        return;
    };
    let config = crate::model::Config::load(
        Some(directory.join("models.json"))
            .filter(|path| path.is_file())
            .as_deref(),
    )
    .ok();
    let wait = budget(config.as_ref());
    let Some((project, route)) = project(directory, &cwd) else {
        return;
    };
    let session = payload["session_id"].as_str().unwrap_or("");
    // Opening the store and choosing the route count in the budget too.
    let left = wait.saturating_sub(started.elapsed());
    let answered = if left.is_zero() {
        Answer::TimedOut
    } else {
        let quick = route == crate::indexed_query::HookRoute::Quick;
        context(directory, &cwd, &project, &question, left, quick)
    };
    let note = note_for(&answered, route, wait)
        .and_then(|(kind, note)| note_once(directory, session, kind, &note));
    let mut printed = match &answered {
        Answer::Packet(markdown) => output(markdown),
        _ => json!({}),
    };
    if let Some(note) = note {
        printed["systemMessage"] = json!(note);
    }
    if printed.as_object().is_some_and(|fields| !fields.is_empty()) {
        let _ = writeln!(std::io::stdout(), "{printed}");
    }
}

/// The note for the person, and its kind, when the hook gave up for time
/// (on a project with reviewed memory, a sync does not help) or answered
/// from the quick packet.
fn note_for(
    answered: &Answer,
    route: crate::indexed_query::HookRoute,
    wait: Duration,
) -> Option<(&'static str, String)> {
    use crate::indexed_query::HookRoute;
    let seconds = wait.as_secs();
    match (answered, route) {
        (Answer::TimedOut, HookRoute::Memory) => Some((
            "memory",
            format!(
                "Graphicle added no project context to this prompt: this project has reviewed memory, so a question reads every file of it, which took longer than {seconds} s. A sync does not change that: raise the budget with `graphicle config set prompt_context.seconds N` (at most {MAX_SECONDS})."
            ),
        )),
        (Answer::TimedOut, _) => Some((
            "time",
            format!(
                "Graphicle added no project context to this prompt: it did not answer within {seconds} s. Run `graphicle sync` in the project if it changed a lot, or raise the budget with `graphicle config set prompt_context.seconds N` (at most {MAX_SECONDS})."
            ),
        )),
        (Answer::Packet(_), HookRoute::Quick) => Some((
            "index",
            "Graphicle's passage index of this project is not ready, so prompts get the files that define the code names they give. Run `graphicle sync` in the project to build it.".to_string(),
        )),
        _ => None,
    }
}

/// What the `graphicle context` the hook runs gave.
#[derive(Debug, PartialEq)]
enum Answer {
    /// The packet's Markdown, at most [`crate::query::PUSH_CHARS`]
    /// characters.
    Packet(String),
    /// It ran past the budget and was stopped.
    TimedOut,
    /// It failed or printed no Markdown.
    Nothing,
}

/// `note` for the person, unless this session (`session`, Claude Code's
/// `session_id`) already had one of this `kind`: recorded in the data folder
/// (`hooks/prompt-context-notes.json`, the sessions of the last 7 days).
fn note_once(directory: &Path, session: &str, kind: &str, note: &str) -> Option<String> {
    if session.is_empty() {
        return Some(note.to_string());
    }
    let path = directory.join("hooks").join("prompt-context-notes.json");
    let now = chrono::Utc::now().timestamp();
    let mut seen: serde_json::Map<String, Value> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    seen.retain(|_, at| at["at"].as_i64().is_some_and(|at| now - at < 7 * 24 * 3600));
    let key = format!("{session}:{kind}");
    if seen.contains_key(&key) {
        return None;
    }
    seen.insert(key, json!({"at": now}));
    let written = std::fs::create_dir_all(path.parent()?).and_then(|_| {
        crate::local::write_private(&path, Value::Object(seen).to_string().as_bytes())
            .map_err(|error| std::io::Error::other(error.message))
    });
    // A note that cannot be recorded is still shown once now.
    let _ = written;
    Some(note.to_string())
}

/// The project whose registered folder holds `cwd`, and how the hook asks
/// it ([`crate::indexed_query::hook_route`]); never the saved default
/// project, so a prompt in another folder gets nothing.
fn project(directory: &Path, cwd: &Path) -> Option<(String, crate::indexed_query::HookRoute)> {
    let store = crate::store::Store::open(directory).ok()?;
    let session = crate::local::session(&store, false).ok()?;
    let project = crate::capture::project_for_cwd(&store, &session, cwd)
        .ok()
        .flatten()?;
    let route = store
        .read(|db| crate::indexed_query::hook_route(db, &project))
        .unwrap_or(crate::indexed_query::HookRoute::Quick);
    Some((project, route))
}

/// `graphicle --data-dir <directory> context --json --project <project> -`
/// in `cwd`, the question on its standard input, at most `wait`: the
/// packet's Markdown, at most [`crate::query::PUSH_CHARS`] characters, or
/// `None` when it fails, prints none or runs past `wait` (it is then
/// stopped).
fn context(
    directory: &Path,
    cwd: &Path,
    project: &str,
    question: &str,
    wait: Duration,
    graph_only: bool,
) -> Answer {
    match context_of(directory, cwd, project, question, wait, graph_only) {
        Ok(Some(markdown)) => Answer::Packet(markdown),
        Ok(None) => Answer::Nothing,
        Err(()) => Answer::TimedOut,
    }
}

/// [`context`]: `Err` when it ran past `wait`.
fn context_of(
    directory: &Path,
    cwd: &Path,
    project: &str,
    question: &str,
    wait: Duration,
    graph_only: bool,
) -> std::result::Result<Option<String>, ()> {
    use std::io::{Read, Write};
    let Some(executable) = std::env::current_exe().ok() else {
        return Ok(None);
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg("--data-dir")
        .arg(directory)
        .args(["context", "--json", "--project", project]);
    if graph_only {
        command.arg("--graph-only");
    }
    command
        .arg("-")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return Ok(None);
    };
    // The question (at most 16,000 bytes, below a pipe's buffer), then the
    // end of the input.
    let written = child
        .stdin
        .take()
        .is_some_and(|mut input| input.write_all(question.as_bytes()).is_ok());
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    }
    let Some(mut stdout) = child.stdout.take() else {
        return Ok(None);
    };
    // Read as it is written: a packet larger than the pipe's buffer would
    // otherwise hold the process up.
    let reader = std::thread::spawn(move || {
        let mut printed = Vec::new();
        stdout.read_to_end(&mut printed).ok().map(|_| printed)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if started.elapsed() < wait => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
            _ => return Ok(None),
        }
    }
    let Some(printed) = reader.join().ok().flatten() else {
        return Ok(None);
    };
    let Some(packet) = serde_json::from_slice::<Value>(&printed).ok() else {
        return Ok(None);
    };
    let markdown = packet["markdown"].as_str().unwrap_or("").trim();
    Ok((!markdown.is_empty()).then(|| markdown.chars().take(crate::query::PUSH_CHARS).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_code_question_of_some_length_gets_context() {
        let ask = |prompt: &str| question(&json!({"prompt": prompt}));
        // Short replies and questions in plain words get nothing.
        for prompt in [
            "",
            "yes",
            "fix `x`",
            "parse_config?",
            "Please summarise what we decided about caching last week",
        ] {
            assert_eq!(ask(prompt), None, "{prompt}");
        }
        assert_eq!(question(&json!({})), None);
        assert_eq!(question(&json!({"prompt": 3})), None);
        // A prompt that names code, or reads like a bug report, does.
        let named = "Why does `parse_config` drop the last line?";
        assert_eq!(ask(named).as_deref(), Some(named));
        let report = "The export crashes with an error when the folder is empty";
        assert_eq!(ask(report).as_deref(), Some(report));
        // A long one is shortened as `context` shortens it.
        let long = format!("`parse_config` fails.\n{}", "word ".repeat(5000));
        let asked = ask(&long).unwrap();
        assert!(asked.len() <= crate::code_question::MAX_QUESTION_BYTES);
        assert!(asked.starts_with("`parse_config` fails."));
    }

    #[test]
    fn the_budget_is_the_variable_else_the_setting_else_ten_seconds() {
        let set = |seconds: u64| crate::model::Config {
            prompt_context: Some(crate::model::PromptContextSettings {
                seconds: Some(seconds),
            }),
            ..Default::default()
        };
        if std::env::var_os(SECONDS_VARIABLE).is_none() {
            assert_eq!(budget(None), Duration::from_secs(SECONDS));
            assert_eq!(budget(Some(&crate::model::Config::default())).as_secs(), 10);
            assert_eq!(budget(Some(&set(4))).as_secs(), 4);
        }
        assert!(set(MAX_SECONDS).validate().is_ok());
        assert!(set(MAX_SECONDS + 1).validate().is_err());
        assert!(set(0).validate().is_err());
    }

    #[test]
    fn the_note_says_what_helps() {
        use crate::indexed_query::HookRoute;
        let wait = Duration::from_secs(10);
        let packet = Answer::Packet("x".into());
        assert_eq!(note_for(&packet, HookRoute::Full, wait), None);
        assert_eq!(note_for(&packet, HookRoute::Memory, wait), None);
        assert_eq!(note_for(&Answer::Nothing, HookRoute::Quick, wait), None);
        let (kind, note) = note_for(&packet, HookRoute::Quick, wait).unwrap();
        assert_eq!(kind, "index");
        assert!(note.contains("graphicle sync"), "{note}");
        let (kind, note) = note_for(&Answer::TimedOut, HookRoute::Full, wait).unwrap();
        assert_eq!(kind, "time");
        assert!(note.contains("within 10 s"), "{note}");
        // Reviewed memory: the full path reads every file, and a sync does
        // not change that.
        let (kind, note) = note_for(&Answer::TimedOut, HookRoute::Memory, wait).unwrap();
        assert_eq!(kind, "memory");
        assert!(note.contains("reviewed memory"), "{note}");
        assert!(note.contains("A sync does not change that"), "{note}");
        assert!(note.contains("prompt_context.seconds"), "{note}");
    }

    #[test]
    fn a_note_is_given_once_per_session_and_kind() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(note_once(dir.path(), "s1", "time", "slow").is_some());
        assert!(note_once(dir.path(), "s1", "time", "slow").is_none());
        assert!(note_once(dir.path(), "s1", "index", "not ready").is_some());
        assert!(note_once(dir.path(), "s2", "time", "slow").is_some());
        // Without a session id there is nothing to remember it by.
        assert!(note_once(dir.path(), "", "time", "slow").is_some());
        assert!(note_once(dir.path(), "", "time", "slow").is_some());
    }

    #[test]
    fn the_output_is_the_markdown_as_additional_context() {
        assert_eq!(
            output("Graphicle project evidence"),
            json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit",
                "additionalContext":"Graphicle project evidence"}})
        );
    }
}
