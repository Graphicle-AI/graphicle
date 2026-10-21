//! Claude Code's prompt-context hook (`graphicle setup --prompt-context`,
//! off by default): on UserPromptSubmit, `graphicle hook claude-code
//! prompt-context` runs `graphicle context` on the prompt and hands Claude
//! Code the packet's Markdown as `additionalContext`, so the agent starts
//! with the retrieval's excerpts. `graphicle context` keeps a packet within
//! [`crate::query::PUSH_CHARS`] (8,400) characters, below the 10,000 that
//! Claude Code shows of a hook's output (it shows a longer one as a 2 KB
//! preview).
//!
//! The hook adds nothing, and never holds the prompt up for long: when the
//! prompt has fewer than [`MIN_CHARS`] characters besides spaces, when it is
//! not a code question ([`crate::code_question::is_code_question`], the
//! classifier `context` itself uses), when the folder is in no Graphicle
//! project, when `graphicle context` fails or prints no Markdown, and when
//! it has not answered within [`SECONDS`] seconds (it is then stopped).
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Characters a prompt needs, spaces left out, to get context: shorter
/// ones ("yes", "go on", "run the tests") are replies, not questions.
pub const MIN_CHARS: usize = 16;

/// Seconds the hook waits for `graphicle context` before it gives up and
/// adds nothing.
pub const SECONDS: u64 = 10;

/// The timeout of the hook's entry in Claude Code's settings, in seconds:
/// a backstop above [`SECONDS`], so Claude Code never waits longer.
pub const HOOK_TIMEOUT: u64 = 15;

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
/// [`output`] when the prompt gets context, else nothing.
pub fn run(directory: &Path, payload: &Value) {
    use std::io::Write;
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
    let Some(project) = project(directory, &cwd) else {
        return;
    };
    if let Some(markdown) = context(
        directory,
        &cwd,
        &project,
        &question,
        Duration::from_secs(SECONDS),
    ) {
        let _ = writeln!(std::io::stdout(), "{}", output(&markdown));
    }
}

/// The project whose registered folder holds `cwd`; never the saved default
/// project, so a prompt in another folder gets nothing.
fn project(directory: &Path, cwd: &Path) -> Option<String> {
    let store = crate::store::Store::open(directory).ok()?;
    let session = crate::local::session(&store, false).ok()?;
    crate::capture::project_for_cwd(&store, &session, cwd)
        .ok()
        .flatten()
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
) -> Option<String> {
    use std::io::{Read, Write};
    let mut command = std::process::Command::new(std::env::current_exe().ok()?);
    command
        .arg("--data-dir")
        .arg(directory)
        .args(["context", "--json", "--project", project, "-"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    // The question (at most 16,000 bytes, below a pipe's buffer), then the
    // end of the input.
    let written = child
        .stdin
        .take()
        .is_some_and(|mut input| input.write_all(question.as_bytes()).is_ok());
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut stdout = child.stdout.take()?;
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
                return None;
            }
            _ => return None,
        }
    }
    let printed = reader.join().ok()??;
    let packet: Value = serde_json::from_slice(&printed).ok()?;
    let markdown = packet["markdown"].as_str()?.trim();
    (!markdown.is_empty()).then(|| markdown.chars().take(crate::query::PUSH_CHARS).collect())
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
    fn the_output_is_the_markdown_as_additional_context() {
        assert_eq!(
            output("Graphicle project evidence"),
            json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit",
                "additionalContext":"Graphicle project evidence"}})
        );
    }
}
