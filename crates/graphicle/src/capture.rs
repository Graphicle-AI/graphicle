//! Agent-session capture: completed turns of Claude Code and Codex
//! transcripts become documents of a per-project "agent sessions" JSONL
//! connector through the ordinary delta import — originals that can be
//! searched, cited and forgotten like any other source, never summaries.
//!
//! The capture policy is explicit and recorded on every document:
//! * `<private>…</private>` spans (and client-injected `<system-reminder>`
//!   blocks) are dropped before anything else sees the text;
//! * credentials are redacted ([`crate::operations::redact`] plus common token
//!   formats);
//! * tool inputs and outputs over [`TOOL_TEXT_CAP`] bytes become
//!   `[tool output: N bytes, sha256 …]`;
//! * everything else is stored verbatim.
//!
//! A turn is one user prompt with everything that answered it; the prompts of
//! Codex's own helper threads (naming a task, consolidating its memories,
//! suggesting what to try) are not a person's and start no turn. Only completed
//! turns are imported, each once (`<client>:<session>:<turn>`); a forgotten
//! turn is never re-imported (the connector's tombstones). `session.forget`
//! forgets a whole session with a receipt and blocks its later turns. Hooks
//! never block the agent: they read the transcript path the client passes and
//! hand the import to a detached process, and they always exit 0. No model is
//! called.
//!
//! The connector is created on first capture and is private to the workspace
//! owner (`workspace_readers: false`); `connector.sharing` can widen it.
//! Pausing it (`connector.state`) stops capture. Deleting it forgets every
//! captured turn, but transcripts still on disk are captured again while the
//! hooks stay installed — `session.forget` is the lasting way to forget.
use crate::{
    Error, Result, id, now,
    store::{Store, one, repo, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Name of the per-project connector captured sessions live in.
pub const CONNECTOR_NAME: &str = "agent sessions";
/// Tool inputs and outputs longer than this are stored as a size and digest.
pub const TOOL_TEXT_CAP: usize = 16 * 1024;
/// Largest rendered turn, kept under the default connector document text
/// limit of earlier builds (512 KiB) so turns render as they always have;
/// the limit itself is `limits.document_text_bytes` (default 8 MiB).
const TURN_CAP: usize = 480 * 1024;
/// Session-start context budget, in characters.
pub const START_CONTEXT_CHARS: usize = 8000;

/// The coding client a transcript comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    Codex,
}

impl Client {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "claude-code" | "claude_code" | "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            _ => Err(Error::bad("Choose the claude-code or codex client")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// What the capture policy did to one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub private_spans: usize,
    pub credentials: usize,
    pub elided: usize,
}

/// One completed turn, rendered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub index: usize,
    pub started_at: Option<String>,
    pub prompt: String,
    pub text: String,
    pub tools: Vec<String>,
    pub files: BTreeSet<String>,
    pub exit_codes: Vec<i64>,
    pub applied: Applied,
}

/// A parsed transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Completed turns only.
    pub turns: Vec<Turn>,
    /// Turns seen, including an unfinished last one.
    pub seen: usize,
}

#[derive(Default)]
struct Draft {
    started_at: Option<String>,
    prompt: String,
    parts: Vec<String>,
    tools: Vec<String>,
    files: BTreeSet<String>,
    exit_codes: Vec<i64>,
    applied: Applied,
    answered: bool,
    open_calls: BTreeSet<String>,
}

/// Remove `<private>` spans and client-injected `<system-reminder>` blocks.
fn strip_private(text: &str, applied: &mut Applied) -> String {
    use std::sync::OnceLock;
    static TAGS: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| {
        regex::Regex::new(
            r"(?is)<(private|system-reminder)\b[^>]*>.*?</(?:private|system-reminder)\s*>",
        )
        .unwrap()
    });
    let mut count = 0;
    let out = tags.replace_all(text, |_: &regex::Captures| {
        count += 1;
        ""
    });
    applied.private_spans += count;
    // An unclosed `<private>` hides everything after it.
    match out.to_ascii_lowercase().find("<private") {
        Some(at) => {
            applied.private_spans += 1;
            out[..at].to_string()
        }
        None => out.into_owned(),
    }
}

/// Redact credentials: the service's key=value rule plus common token formats.
fn redact(text: &str, applied: &mut Applied) -> String {
    use std::sync::OnceLock;
    static TOKENS: OnceLock<regex::Regex> = OnceLock::new();
    let tokens = TOKENS.get_or_init(|| {
        regex::Regex::new(concat!(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"|\bAKIA[0-9A-Z]{16}\b",
            r"|\bgh[opsu]_[A-Za-z0-9]{36}\b|\bgithub_pat_[A-Za-z0-9_]{60,}\b",
            r"|\bsk-ant-[A-Za-z0-9_-]{20,}|\bsk-[A-Za-z0-9_-]{20,}",
            r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
            r"|\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            r"|\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
            r"|\bAIza[0-9A-Za-z_-]{35}\b",
            r"|\bglpat-[A-Za-z0-9_-]{20,}",
        ))
        .unwrap()
    });
    const REDACTED: &str = "[credential redacted]";
    // Keyed values first, so `api_key=<token>` is replaced whole.
    let keyed = crate::operations::redact(text);
    let mut count = keyed
        .matches(REDACTED)
        .count()
        .saturating_sub(text.matches(REDACTED).count());
    let out = tokens
        .replace_all(&keyed, |_: &regex::Captures| {
            count += 1;
            REDACTED
        })
        .into_owned();
    applied.credentials += count;
    out
}

/// Apply the capture policy to free text (prompts, answers, work-state values).
pub(crate) fn clean(text: &str, applied: &mut Applied) -> String {
    redact(&strip_private(text, applied), applied)
}

/// Apply the policy to a tool input or output: clean, then elide past the cap.
fn tool_text(kind: &str, text: &str, applied: &mut Applied) -> String {
    let text = clean(text, applied);
    if text.len() <= TOOL_TEXT_CAP {
        return text;
    }
    applied.elided += 1;
    format!(
        "[tool {kind}: {} bytes, sha256 {}]",
        text.len(),
        graphicle_memory::text::sha256(text.as_bytes())
    )
}

/// Paths a tool call names (`file_path`, `path`, `notebook_path`, patch headers).
fn paths_of(input: &Value, files: &mut BTreeSet<String>) {
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(path) = input[key]
            .as_str()
            .filter(|p| !p.is_empty() && p.len() <= 1000)
        {
            files.insert(path.to_string());
        }
    }
    let text = match input {
        Value::String(text) => text.as_str(),
        _ => input["input"]
            .as_str()
            .or(input["patch"].as_str())
            .unwrap_or(""),
    };
    for line in text.lines() {
        for marker in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            if let Some(path) = line
                .strip_prefix(marker)
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                files.insert(path.to_string());
            }
        }
    }
}

fn block_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                _ => part["text"].as_str().map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn compact(input: &Value) -> String {
    match input {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn finish(draft: Draft, index: usize) -> Turn {
    let mut text = draft.parts.join("\n");
    if text.len() > TURN_CAP {
        let mut cut = TURN_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[turn truncated at 480 KiB]");
    }
    Turn {
        index,
        started_at: draft.started_at,
        prompt: draft.prompt,
        text,
        tools: draft.tools,
        files: draft.files,
        exit_codes: draft.exit_codes,
        applied: draft.applied,
    }
}

/// Collect turns: `drafts` in order; all but the last are complete, the last
/// when it was answered and every tool call it made returned.
fn close(drafts: Vec<Draft>, session: &mut Session) {
    session.seen = drafts.len();
    let total = drafts.len();
    for (position, draft) in drafts.into_iter().enumerate() {
        let last = position + 1 == total;
        if last && (!draft.answered || !draft.open_calls.is_empty()) {
            break;
        }
        session.turns.push(finish(draft, position + 1));
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Parse a Claude Code transcript (`~/.claude/projects/<dir>/<session>.jsonl`).
/// Sidechain (sub-agent) and meta records are skipped, a protocol notice
/// ([`protocol_payload`]) does not start a turn, and thinking blocks are
/// never stored.
pub fn parse_claude_code(text: &str) -> Result<Session> {
    let mut session = Session::default();
    let mut drafts: Vec<Draft> = Vec::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = record["sessionId"].as_str()
            && session.session_id.is_empty()
        {
            session.session_id = id.to_string();
        }
        if let Some(cwd) = record["cwd"].as_str().filter(|c| !c.is_empty()) {
            session.cwd = Some(cwd.to_string());
        }
        if let Some(branch) = record["gitBranch"].as_str().filter(|b| !b.is_empty()) {
            session.branch = Some(branch.to_string());
        }
        if record["isSidechain"] == true || record["isMeta"] == true {
            continue;
        }
        let message = &record["message"];
        let role = message["role"]
            .as_str()
            .or(record["type"].as_str())
            .unwrap_or("");
        let stamp = record["timestamp"].as_str().map(str::to_string);
        match role {
            "user" => {
                let content = &message["content"];
                let mut prompt = Vec::new();
                let mut results = Vec::new();
                match content {
                    Value::String(text) => prompt.push(text.clone()),
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("tool_result") => results.push(block),
                                Some("text") => {
                                    prompt.push(block["text"].as_str().unwrap_or("").to_string())
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(draft) = drafts.last_mut() {
                    for result in results {
                        let id = s(result, "tool_use_id").to_string();
                        draft.open_calls.remove(&id);
                        let name = names.get(&id).cloned().unwrap_or_else(|| "tool".into());
                        let output = block_text(&result["content"]);
                        let marker = if result["is_error"] == true {
                            " (error)"
                        } else {
                            ""
                        };
                        let body = tool_text("output", &output, &mut draft.applied);
                        draft
                            .parts
                            .push(format!("tool {name} output{marker}: {body}"));
                    }
                }
                let prompt = prompt.join("\n");
                if !prompt.trim().is_empty() && !protocol_payload(&prompt) {
                    let mut draft = Draft {
                        started_at: stamp,
                        ..Draft::default()
                    };
                    let cleaned = clean(&prompt, &mut draft.applied);
                    draft.prompt = cleaned.trim().to_string();
                    draft.parts.push(format!("user: {}", cleaned.trim()));
                    drafts.push(draft);
                }
            }
            "assistant" => {
                let Some(draft) = drafts.last_mut() else {
                    continue;
                };
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text =
                                clean(block["text"].as_str().unwrap_or(""), &mut draft.applied);
                            if !text.trim().is_empty() {
                                draft.parts.push(format!("assistant: {}", text.trim()));
                                draft.answered = true;
                            }
                        }
                        Some("tool_use") => {
                            let name = s(block, "name").to_string();
                            let id = s(block, "id").to_string();
                            paths_of(&block["input"], &mut draft.files);
                            let input =
                                tool_text("input", &compact(&block["input"]), &mut draft.applied);
                            draft.parts.push(format!("tool {name}: {input}"));
                            if !draft.tools.contains(&name) {
                                draft.tools.push(name.clone());
                            }
                            if !id.is_empty() {
                                names.insert(id.clone(), name);
                                draft.open_calls.insert(id);
                            }
                            draft.answered = true;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !valid_session_id(&session.session_id) {
        return Err(Error::bad("Transcript names no valid session id"));
    }
    close(drafts, &mut session);
    Ok(session)
}

/// Whether a user message is only a client's protocol notice — a single
/// `<task-notification>` block, which Claude Code sends when a background
/// task ends — rather than something a person typed (claude-mem's
/// `isInternalProtocolPayload` in `src/utils/tag-stripping.ts`). A notice
/// with anything around it, a nested notice, or a message over 256 KiB is
/// left as a prompt. Any client.
fn protocol_payload(text: &str) -> bool {
    const OPEN: &str = "<task-notification";
    const CLOSE: &str = "</task-notification";
    let boundary = |rest: &str| {
        !rest
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if text.len() > 256 * 1024 {
        return false;
    }
    let Some(rest) = text.trim_start().strip_prefix(OPEN) else {
        return false;
    };
    if !boundary(rest) {
        return false;
    }
    let Some(end) = rest.find('>') else {
        return false;
    };
    let body = &rest[end + 1..];
    for (at, _) in body.match_indices('<') {
        let tail = &body[at..];
        if let Some(after) = tail.strip_prefix(OPEN)
            && boundary(after)
        {
            return false;
        }
        if let Some(after) = tail.strip_prefix(CLOSE)
            && boundary(after)
        {
            return after
                .strip_prefix('>')
                .is_some_and(|left| left.trim().is_empty());
        }
    }
    false
}

/// Whether a Codex user message is the prompt of one of Codex's own helper
/// threads — naming a task, consolidating memories, suggesting what to try —
/// which arrive the way a person's prompt does. Each pattern is the helper's
/// opening line, never something a person would type (claude-mem ffdff16,
/// `src/utils/tag-stripping.ts`). Codex only: from another client the same
/// text is a person's prompt.
fn codex_helper_prompt(text: &str) -> bool {
    use std::sync::OnceLock;
    static OPENINGS: OnceLock<regex::Regex> = OnceLock::new();
    OPENINGS
        .get_or_init(|| {
            regex::Regex::new(concat!(
                r"^(?:You are a helpful assistant\. You will be presented with a user prompt, ",
                r"and your job is to provide a short title",
                r"|## Memory Writing Agent: Phase 2 \(Consolidation\)",
                r"|# Overview\r?\n(?:\r?\n)?Generate 0 to 3 hyperpersonalized suggestions)",
            ))
            .unwrap()
        })
        .is_match(text.trim_start())
}

/// Parse a Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`).
/// Environment, instruction, skill and plugin preambles and protocol notices
/// ([`protocol_payload`]) are not user prompts; a helper
/// thread's prompt ([`codex_helper_prompt`]) and everything answering it are
/// not captured; reasoning is never stored.
pub fn parse_codex(text: &str) -> Result<Session> {
    let mut session = Session::default();
    let mut drafts: Vec<Draft> = Vec::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut fallback: Vec<(String, String, Option<String>)> = Vec::new();
    let mut messages = 0usize;
    // Inside a helper prompt's turn: skipped up to the next user prompt.
    let mut helper = false;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &record["payload"];
        let stamp = record["timestamp"].as_str().map(str::to_string);
        match record["type"].as_str().unwrap_or("") {
            "session_meta" => {
                if session.session_id.is_empty()
                    && let Some(id) = payload["id"].as_str().or(payload["session_id"].as_str())
                {
                    session.session_id = id.to_string();
                }
                if let Some(cwd) = payload["cwd"].as_str() {
                    session.cwd = Some(cwd.to_string());
                }
                if let Some(branch) = payload["git"]["branch"].as_str() {
                    session.branch = Some(branch.to_string());
                }
            }
            "turn_context" => {
                if session.cwd.is_none()
                    && let Some(cwd) = payload["cwd"].as_str()
                {
                    session.cwd = Some(cwd.to_string());
                }
            }
            "response_item" => match payload["type"].as_str().unwrap_or("") {
                "message" => {
                    let role = payload["role"].as_str().unwrap_or("");
                    let text = block_text(&payload["content"]);
                    let trimmed = text.trim_start();
                    if role == "user" {
                        if trimmed.is_empty()
                            || [
                                "<environment_context",
                                "<user_instructions",
                                "<permissions",
                                "<INSTRUCTIONS",
                                "<skills_instructions",
                                "<recommended_plugins",
                            ]
                            .iter()
                            .any(|p| trimmed.starts_with(p))
                            || protocol_payload(&text)
                        {
                            continue;
                        }
                        messages += 1;
                        helper = codex_helper_prompt(trimmed);
                        if helper {
                            continue;
                        }
                        let mut draft = Draft {
                            started_at: stamp,
                            ..Draft::default()
                        };
                        let cleaned = clean(&text, &mut draft.applied);
                        draft.prompt = cleaned.trim().to_string();
                        draft.parts.push(format!("user: {}", cleaned.trim()));
                        drafts.push(draft);
                    } else if role == "assistant"
                        && !helper
                        && let Some(draft) = drafts.last_mut()
                        && !trimmed.is_empty()
                    {
                        messages += 1;
                        let cleaned = clean(&text, &mut draft.applied);
                        draft.parts.push(format!("assistant: {}", cleaned.trim()));
                        draft.answered = true;
                    }
                }
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    let Some(draft) = drafts.last_mut().filter(|_| !helper) else {
                        continue;
                    };
                    let name = payload["name"].as_str().unwrap_or("shell").to_string();
                    let input = payload
                        .get("arguments")
                        .or(payload.get("input"))
                        .or(payload.get("action"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let parsed = input
                        .as_str()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok())
                        .unwrap_or_else(|| input.clone());
                    paths_of(&parsed, &mut draft.files);
                    paths_of(&input, &mut draft.files);
                    let shown = tool_text("input", &compact(&input), &mut draft.applied);
                    draft.parts.push(format!("tool {name}: {shown}"));
                    if !draft.tools.contains(&name) {
                        draft.tools.push(name.clone());
                    }
                    if let Some(id) = payload["call_id"].as_str() {
                        names.insert(id.to_string(), name);
                        draft.open_calls.insert(id.to_string());
                    }
                    draft.answered = true;
                }
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                    let Some(draft) = drafts.last_mut().filter(|_| !helper) else {
                        continue;
                    };
                    let id = payload["call_id"].as_str().unwrap_or("").to_string();
                    draft.open_calls.remove(&id);
                    let name = names.get(&id).cloned().unwrap_or_else(|| "tool".into());
                    let raw = &payload["output"];
                    let structured = raw
                        .as_str()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok())
                        .unwrap_or_else(|| raw.clone());
                    if let Some(code) = structured["metadata"]["exit_code"].as_i64() {
                        draft.exit_codes.push(code);
                    }
                    let output = match structured.get("output") {
                        Some(Value::String(text)) => text.clone(),
                        _ => block_text(raw),
                    };
                    let body = tool_text("output", &output, &mut draft.applied);
                    draft.parts.push(format!("tool {name} output: {body}"));
                }
                _ => {}
            },
            "event_msg" => {
                let kind = payload["type"].as_str().unwrap_or("");
                if let Some(message) = payload["message"].as_str()
                    && (kind == "user_message" || kind == "agent_message")
                {
                    fallback.push((kind.to_string(), message.to_string(), stamp));
                }
            }
            _ => {}
        }
    }
    // Older rollouts carry the conversation only as events.
    if messages == 0 {
        for (kind, message, stamp) in fallback {
            if kind == "user_message" {
                if protocol_payload(&message) {
                    continue;
                }
                helper = codex_helper_prompt(&message);
                if helper {
                    continue;
                }
                let mut draft = Draft {
                    started_at: stamp,
                    ..Draft::default()
                };
                let cleaned = clean(&message, &mut draft.applied);
                draft.prompt = cleaned.trim().to_string();
                draft.parts.push(format!("user: {}", cleaned.trim()));
                drafts.push(draft);
            } else if !helper && let Some(draft) = drafts.last_mut() {
                let cleaned = clean(&message, &mut draft.applied);
                draft.parts.push(format!("assistant: {}", cleaned.trim()));
                draft.answered = true;
            }
        }
    }
    if !valid_session_id(&session.session_id) {
        return Err(Error::bad("Rollout names no valid session id"));
    }
    close(drafts, &mut session);
    Ok(session)
}

pub fn parse(client: Client, text: &str) -> Result<Session> {
    match client {
        Client::ClaudeCode => parse_claude_code(text),
        Client::Codex => parse_codex(text),
    }
}

/// The policy recorded on every captured document.
pub fn capture_policy(applied: &Applied) -> Value {
    json!({
        "private_spans": "dropped",
        "credentials": "redacted",
        "tool_text_cap_bytes": TOOL_TEXT_CAP,
        "otherwise": "verbatim",
        "private_spans_dropped": applied.private_spans,
        "credentials_redacted": applied.credentials,
        "tool_texts_elided": applied.elided,
    })
}

/// Connector document id of a turn.
pub fn turn_id(client: Client, session: &str, turn: usize) -> String {
    format!("{}:{session}:{turn:04}", client.as_str())
}

/// JSONL-import documents for every completed turn.
pub fn documents(client: Client, session: &Session) -> Vec<Value> {
    session
        .turns
        .iter()
        .map(|turn| {
            let short: String = session.session_id.chars().take(8).collect();
            let mut prompt: String = turn.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            if prompt.chars().count() > 80 {
                prompt = prompt.chars().take(80).collect::<String>() + "…";
            }
            let stamp = turn
                .started_at
                .as_deref()
                .filter(|t| graphicle_memory::temporal::instant(t).is_ok());
            json!({
                "id": turn_id(client, &session.session_id, turn.index),
                "title": format!("{} session {short} · turn {} · {prompt}", client.label(), turn.index),
                "content": turn.text,
                "source_timestamp": stamp,
                "metadata": {
                    "client": client.as_str(),
                    "session_id": session.session_id,
                    "turn": turn.index,
                    "cwd": session.cwd,
                    "git_branch": session.branch,
                    "files_touched": turn.files,
                    "tools": turn.tools,
                    "exit_codes": turn.exit_codes,
                    "capture_policy": capture_policy(&turn.applied),
                },
            })
        })
        .collect()
}

/// `(client, session, turn)` of a captured document id.
fn turn_of(document: &str) -> Option<(&str, &str, usize)> {
    let (rest, turn) = document.rsplit_once(':')?;
    let (client, session) = rest.split_once(':')?;
    Some((client, session, turn.parse().ok()?))
}

/// Tombstone name that blocks every later turn of a forgotten session.
fn session_marker(client: Client, session: &str) -> String {
    format!("{}:{session}:*", client.as_str())
}

/// Session-wide tombstone to check in the same transaction as a turn import.
pub(crate) fn document_session_marker(document: &str) -> Option<String> {
    let (client, session, _) = turn_of(document)?;
    let parsed = Client::parse(client).ok()?;
    (parsed.as_str() == client).then(|| session_marker(parsed, session))
}

/// The repository's agent-sessions connector (`id`, `state`), if any.
fn find_connector(db: &Connection, repository: &str) -> Result<Option<Value>> {
    one(
        db,
        "SELECT id,state FROM am_connectors WHERE repository_id=? AND kind='jsonl' AND name=? ORDER BY created_at,id LIMIT 1",
        params![repository, CONNECTOR_NAME],
    )
}

impl Store {
    /// The project's agent-sessions connector and its state. Created on first
    /// use inside the lookup's write transaction, so concurrent hooks agree
    /// on one connector; creating it needs admin permission.
    pub fn agent_connector(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
    ) -> Result<(String, String)> {
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            if let Some(row) = find_connector(db, identifier)? {
                return Ok((s(&row, "id").to_string(), s(&row, "state").to_string()));
            }
            if !p.permits("admin") {
                return Err(Error::denied());
            }
            let created = id();
            db.execute(
                "INSERT INTO am_connectors(id,repository_id,name,kind,config,credential_env,state,created_at) VALUES(?,?,?,'jsonl',?,NULL,'active',?)",
                params![
                    created,
                    identifier,
                    CONNECTOR_NAME,
                    json!({"acl":{"workspace_readers":false,"credentials":[]}}).to_string(),
                    now()
                ],
            )?;
            Ok((created, "active".to_string()))
        })
    }

    /// Import the completed turns of one transcript that are not stored yet.
    pub fn capture_transcript(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        client: Client,
        transcript: &str,
    ) -> Result<Value> {
        self.capture_session(
            workspace,
            token,
            repository,
            client,
            &parse(client, transcript)?,
        )
    }

    /// [`Store::capture_transcript`] for an already parsed transcript.
    pub fn capture_session(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        client: Client,
        session: &Session,
    ) -> Result<Value> {
        if !valid_session_id(&session.session_id) {
            return Err(Error::bad("Invalid session id"));
        }
        let (connector, state) = self.agent_connector(workspace, token, repository)?;
        let prefix = format!("{}:{}:", client.as_str(), session.session_id);
        let (stored, blocked): (BTreeMap<String, String>, bool) = self.authorized_read(workspace, token, "collect", |db, _| {
            let stored = rows(
                db,
                "SELECT id,sha256 FROM am_connector_docs WHERE connector_id=? AND substr(id,1,length(?2))=?2",
                params![connector, prefix],
            )?
            .into_iter()
            .map(|r| (s(&r, "id").to_string(), s(&r, "sha256").to_string()))
            .collect();
            let blocked = crate::deletion::forgotten(db, &connector, &session_marker(client, &session.session_id))?;
            Ok((stored, blocked))
        })?;
        let documents = documents(client, session);
        let total = documents.len();
        let fresh: Vec<Value> = documents
            .into_iter()
            .filter(|d| {
                stored.get(s(d, "id")).map(String::as_str)
                    != Some(graphicle_memory::text::sha256(s(d, "content").as_bytes()).as_str())
            })
            .collect();
        let applied = session.turns.iter().fold(Applied::default(), |mut all, t| {
            all.private_spans += t.applied.private_spans;
            all.credentials += t.applied.credentials;
            all.elided += t.applied.elided;
            all
        });
        let mut out = json!({
            "client": client.as_str(),
            "session_id": session.session_id,
            "cwd": session.cwd,
            "connector_id": connector,
            "turns_seen": session.seen,
            "turns_completed": total,
            "unchanged": total - fresh.len(),
            "imported": 0,
            "forgotten_skipped": 0,
            "session_forgotten": blocked,
            "paused": state != "active",
            "capture_policy": capture_policy(&applied),
        });
        if blocked || state != "active" {
            out["unchanged"] = json!(0);
            return Ok(out);
        }
        // Short import transactions: at most 200 turns or 16 MiB each.
        let (mut imported, mut skipped) = (0, 0);
        let mut batch: Vec<Value> = Vec::new();
        let mut bytes = 0;
        let mut pending = fresh.into_iter().peekable();
        while let Some(document) = pending.next() {
            bytes += s(&document, "content").len();
            batch.push(document);
            let full = batch.len() >= 200
                || pending
                    .peek()
                    .is_some_and(|next| bytes + s(next, "content").len() > 16 * 1024 * 1024);
            if full || pending.peek().is_none() {
                let result = self.import_documents(
                    workspace,
                    token,
                    &json!({"repository_id":repository,"connector_id":connector,"documents":std::mem::take(&mut batch),"mode":"delta"}),
                )?;
                imported += result["imported"].as_u64().unwrap_or(0);
                skipped += result["forgotten_skipped"].as_u64().unwrap_or(0);
                bytes = 0;
            }
        }
        out["imported"] = json!(imported);
        out["forgotten_skipped"] = json!(skipped);
        Ok(out)
    }

    /// `session.forget`: forget every captured turn of a session through
    /// certified deletion (one receipt per batch) and block its later turns.
    pub fn forget_session(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let repository = crate::string(args, "repository_id", 100)?.to_string();
        let client = Client::parse(crate::string(args, "client", 30)?)?;
        let session = crate::string(args, "session_id", 100)?;
        if !valid_session_id(session) {
            return Err(Error::bad("Invalid session id"));
        }
        let (connector, _) = self.agent_connector(workspace, token, &repository)?;
        let prefix = format!("{}:{session}:", client.as_str());
        let mut forgotten = 0;
        let mut receipts = Vec::new();
        // Imports check the session block inside their write transaction,
        // including batches prepared before this call. Drain existing turns
        // in bounded receipts after installing that block.
        for round in 0..3 {
            let ids: Vec<String> = self.authorized(workspace, token, "admin", |db, p| {
                repo(db, p, &repository)?;
                if round == 0 {
                    crate::deletion::block(db, &connector, &session_marker(client, session))?;
                }
                Ok(rows(
                    db,
                    "SELECT id FROM am_connector_docs WHERE connector_id=? AND substr(id,1,length(?2))=?2 ORDER BY id",
                    params![connector, prefix],
                )?
                .iter()
                .map(|r| s(r, "id").to_string())
                .collect())
            })?;
            if ids.is_empty() {
                break;
            }
            for batch in ids.chunks(crate::deletion::MAX_SCOPE_DOCUMENTS) {
                let result = self.maintain(
                    workspace,
                    token,
                    "source.forget",
                    &json!({"repository_id":repository,"connector_id":connector,"document_ids":batch}),
                )?;
                receipts.push(result["receipt"].clone());
            }
            forgotten += ids.len();
        }
        Ok(
            json!({"forgotten_turns":forgotten,"session_id":session,"client":client.as_str(),
            "later_turns_blocked":true,"receipt":receipts.last().cloned().unwrap_or(Value::Null),"receipts":receipts}),
        )
    }

    /// `session.timeline` (read): the captured turns of a session (the latest
    /// by default) around an anchor turn, as an index — title, time and
    /// source id; `read_project_source` returns the details.
    pub fn session_timeline(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let before = crate::integer(args, "before", 3, 20)?;
        let after = crate::integer(args, "after", 3, 20)?;
        self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let Some(connector) = find_connector(db, s(&repository, "id"))? else {
                return Ok(json!({"sessions":[],"turns":[],"captured":false}));
            };
            let prefix = format!("connector:{}:", s(&connector, "id"));
            let mut turns: Vec<(String, String, usize, String, Value)> = Vec::new();
            for doc in crate::memory::documents(db, &repository, p)? {
                let Some(document) = s(&doc, "id").strip_prefix(&prefix) else {
                    continue;
                };
                let Some((client, session, turn)) = turn_of(document) else {
                    continue;
                };
                turns.push((
                    session.to_string(),
                    client.to_string(),
                    turn,
                    s(&doc, "id").to_string(),
                    json!({"source_id":doc["id"],"turn":turn,"title":doc["title"],"time":doc["recorded_at"]}),
                ));
            }
            let mut sessions: BTreeMap<String, (String, Value, usize)> = BTreeMap::new();
            for (session, client, _, _, entry) in &turns {
                let slot = sessions
                    .entry(session.clone())
                    .or_insert((client.clone(), Value::Null, 0));
                slot.2 += 1;
                if entry["time"].as_str() > slot.1.as_str() {
                    slot.1 = entry["time"].clone();
                }
            }
            let anchor = args["anchor_source_id"].as_str();
            let chosen = args["session_id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    anchor.and_then(|a| turns.iter().find(|t| t.3 == a).map(|t| t.0.clone()))
                })
                .or_else(|| {
                    sessions
                        .iter()
                        .max_by(|a, b| a.1.1.as_str().cmp(&b.1.1.as_str()).then(a.0.cmp(b.0)))
                        .map(|(id, _)| id.clone())
                });
            let mut listed: Vec<&(String, String, usize, String, Value)> = turns
                .iter()
                .filter(|t| Some(&t.0) == chosen.as_ref())
                .collect();
            listed.sort_by_key(|t| t.2);
            let center = anchor
                .and_then(|a| listed.iter().position(|t| t.3 == a))
                .unwrap_or(listed.len().saturating_sub(1));
            let from = center.saturating_sub(before);
            let to = (center + after + 1).min(listed.len());
            let mut recent: Vec<Value> = sessions
                .iter()
                .map(|(id, (client, time, count))| json!({"session_id":id,"client":client,"latest":time,"turns":count}))
                .collect();
            recent.sort_by(|a, b| b["latest"].as_str().cmp(&a["latest"].as_str()));
            recent.truncate(10);
            Ok(json!({
                "captured": true,
                "session_id": chosen,
                "turns": listed[from..to].iter().map(|t| t.4.clone()).collect::<Vec<_>>(),
                "turns_in_session": listed.len(),
                "sessions": recent,
                "details": "read_project_source with a source_id returns the full turn",
            }))
        })
    }

    /// Session-start context: open work-state items when an agent recorded
    /// any (labelled as unreviewed), reviewed, current memories (title and
    /// id), the stale count and the last captured turns, within
    /// [`START_CONTEXT_CHARS`]. Drafts and observer notes never appear.
    pub fn session_start_context(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
    ) -> Result<String> {
        let (open_work, memories, stale, recent) =
            self.authorized_read(workspace, token, "read", |db, p| {
                let repository = repo(db, p, repository)?;
                let open_work = crate::work_state::start_section(
                    db,
                    s(&repository, "id"),
                    chrono::Utc::now().timestamp_millis(),
                )?;
                let mut reviewed = Vec::new();
                let mut stale = 0usize;
                for memory in crate::memory::list(db, &repository, p, false)? {
                    if memory["state"] != "reviewed" {
                        continue;
                    }
                    if memory["stale"] == true {
                        stale += 1;
                        continue;
                    }
                    reviewed.push((
                        s(&memory, "title").to_string(),
                        s(&memory, "id").to_string(),
                    ));
                }
                let connector = find_connector(db, s(&repository, "id"))?;
                let mut turns: Vec<(String, String, String)> = Vec::new();
                if let Some(connector) = connector {
                    let prefix = format!("connector:{}:", s(&connector, "id"));
                    for doc in crate::memory::documents(db, &repository, p)? {
                        if s(&doc, "id").starts_with(&prefix) {
                            turns.push((
                                s(&doc, "recorded_at").to_string(),
                                s(&doc, "title").to_string(),
                                s(&doc, "id").to_string(),
                            ));
                        }
                    }
                }
                turns.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.cmp(&a.2)));
                turns.truncate(5);
                Ok((open_work, reviewed, stale, turns))
            })?;
        let mut out = String::from(
            "Graphicle project memory. Reviewed memories are current; cite sources with read_project_source before relying on them.\n",
        );
        let push = |out: &mut String, line: String| {
            if out.chars().count() + line.chars().count() < START_CONTEXT_CHARS {
                out.push_str(&line);
                out.push('\n');
                true
            } else {
                false
            }
        };
        if let Some(section) = open_work {
            push(&mut out, section);
        }
        if memories.is_empty() {
            push(&mut out, "No reviewed memories yet.".into());
        } else {
            push(&mut out, "Reviewed memories:".into());
            for (title, id) in &memories {
                if !push(&mut out, format!("- {title} (memory {id})")) {
                    push(&mut out, "- … more in project_memory".into());
                    break;
                }
            }
        }
        if stale > 0 {
            push(
                &mut out,
                format!(
                    "{stale} reviewed memories are stale (their sources changed) and are left out."
                ),
            );
        }
        if !recent.is_empty() {
            push(
                &mut out,
                "Recent captured turns (session_timeline for more):".into(),
            );
            for (time, title, id) in &recent {
                push(&mut out, format!("- {time} {title} ({id})"));
            }
        }
        Ok(out)
    }
}

/// The project whose registered folder contains `cwd` (nearest ancestor,
/// through recorded folders and `local:` keys: [`crate::folders`]). Never the
/// saved default: a session outside every registered folder is not captured,
/// and neither is one in a folder whose project was never synced (one
/// `graphicle pr` registered) or is not readable with this access key.
pub fn project_for_cwd(
    store: &Store,
    session: &crate::local::Session,
    cwd: &std::path::Path,
) -> Result<Option<String>> {
    Ok(store
        .project_for_path(&session.workspace, &session.capability, cwd)?
        .map(|found| found.id))
}

/// Transcript files under `root` (`*.jsonl`), newest first, at most `limit`.
pub fn transcripts(
    root: &std::path::Path,
    since: Option<std::time::SystemTime>,
    limit: usize,
) -> Vec<std::path::PathBuf> {
    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(path);
            } else if meta.is_file()
                && path.extension().is_some_and(|e| e == "jsonl")
                && meta.len() <= crate::limits::current().transcript_bytes as u64
            {
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                if since.is_none_or(|s| modified >= s) {
                    found.push((modified, path));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.truncate(limit);
    found.into_iter().map(|(_, p)| p).collect()
}

/// Default transcript root of a client.
pub fn default_root(client: Client) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match client {
        Client::ClaudeCode => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(|d| std::path::PathBuf::from(d).join("projects"))
            .or_else(|| home.map(|h| h.join(".claude/projects"))),
        Client::Codex => std::env::var_os("CODEX_HOME")
            .map(|d| std::path::PathBuf::from(d).join("sessions"))
            .or_else(|| home.map(|h| h.join(".codex/sessions"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_strips_private_spans_redacts_and_elides() {
        let mut applied = Applied::default();
        let text = clean(
            "keep <private>my diagnosis</private> token ghp_0123456789abcdefghijklmnopqrstuvwxyz and password=hunter2 <system-reminder>noise</system-reminder>",
            &mut applied,
        );
        assert!(!text.contains("diagnosis") && !text.contains("noise"));
        assert!(
            !text.contains("ghp_") && !text.contains("hunter2"),
            "{text}"
        );
        assert_eq!((applied.private_spans, applied.credentials), (2, 2));
        let unclosed = clean("public <private> everything after", &mut applied);
        assert_eq!(unclosed.trim(), "public");
        let long = "x".repeat(TOOL_TEXT_CAP + 1);
        let elided = tool_text("output", &long, &mut applied);
        assert!(
            elided.starts_with("[tool output: 16385 bytes, sha256 "),
            "{elided}"
        );
        assert_eq!(applied.elided, 1);
    }

    #[test]
    fn only_a_lone_task_notification_is_a_protocol_notice() {
        let notice = "<task-notification>\n<task-id>b7</task-id>\n<status>completed</status>\n</task-notification>";
        for yes in [
            notice.to_string(),
            format!("  \n{notice}\n\t"),
            "<task-notification id=\"b7\" kind='bash'><summary>done</summary></task-notification>"
                .to_string(),
            "<task-notification></task-notification>".to_string(),
            "<task-notification-v2>done</task-notification>".to_string(),
        ] {
            assert!(protocol_payload(&yes), "{yes:?}");
        }
        for no in [
            String::new(),
            "Run the tests".to_string(),
            format!("{notice}\nNow read its output."),
            format!("About this: {notice}"),
            "<task-notifications>done</task-notifications>".to_string(),
            "<task-notification_x>done</task-notification>".to_string(),
            "<task-notification>done</task-notification >".to_string(),
            "<task-notification>done".to_string(),
            "<task-notification>a<task-notification>b</task-notification></task-notification>"
                .to_string(),
            "<task-notification>a</task-notification><task-notification>b</task-notification>"
                .to_string(),
            "<task-notification".to_string(),
            format!(
                "<task-notification>{}</task-notification>",
                "x".repeat(256 * 1024)
            ),
        ] {
            assert!(!protocol_payload(&no), "{no:?}");
        }
        let edge = format!(
            "<task-notification>{}</task-notification>",
            "x".repeat(256 * 1024 - 39)
        );
        assert_eq!(edge.len(), 256 * 1024);
        assert!(protocol_payload(&edge));
    }
}
