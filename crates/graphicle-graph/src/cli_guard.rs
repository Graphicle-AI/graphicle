// Adapted from Graphify 0.9.102 (59b5ab11), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
//! Native assistant hook guards and session/query stamps.
use serde_json::{Map, Value, json};
use std::{
    fs::{self, OpenOptions},
    path::Path,
    sync::LazyLock,
    time::{Duration, SystemTime},
};
static DATA: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/cli/constants.json")).unwrap());
fn out() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::paths::OUT_DIR.as_str())
}
const DEFAULT_NUDGE_GRAPH: &str = ".graphicle/graph.json";
/// Name the effective graph.json in a hook-guard reminder (Graphify #4040).
/// The reminder constants spell the default `.graphicle/graph.json`; with
/// the default output dir the nudge is returned byte-identical.
fn nudge_for_out(nudge: String) -> String {
    nudge_for_out_with(nudge, crate::paths::OUT_DIR.as_str())
}
fn nudge_for_out_with(nudge: String, out_dir: &str) -> String {
    let mut raw = format!("{out_dir}/graph.json");
    if cfg!(windows) {
        raw = raw.replace('\\', "/");
    }
    let reference = crate::path_syntax::posix_join(&crate::path_syntax::posix_parts(&raw));
    if reference == DEFAULT_NUDGE_GRAPH {
        return nudge;
    }
    let Ok(mut d) = serde_json::from_str::<Value>(&nudge) else {
        return nudge;
    };
    let Some(context) = d
        .pointer_mut("/hookSpecificOutput/additionalContext")
        .and_then(|v| {
            v.as_str()
                .map(|s| s.replace(DEFAULT_NUDGE_GRAPH, &reference))
        })
    else {
        return nudge;
    };
    d["hookSpecificOutput"]["additionalContext"] = Value::String(context);
    format!("{d}\n")
}
/// Graph nodes a hint lists at most (0.9.89, #4368).
const HINT_MAX_NODES: usize = 5;
/// A bigger graph.json gets the plain nudge: the label scan must stay fast
/// enough for a per-call hook.
const HINT_GRAPH_MAX_BYTES: u64 = 24 * 1024 * 1024;
/// Declaration keywords a pattern like `def load_config` carries; never the target.
const HINT_KEYWORDS: &[&str] = &[
    "def",
    "class",
    "function",
    "func",
    "const",
    "let",
    "var",
    "import",
    "export",
    "async",
    "return",
    "struct",
    "interface",
    "type",
    "enum",
    "impl",
    "pub",
];
/// grep/rg/ag/ack options whose value is the next argument, never the pattern.
const HINT_VALUE_FLAGS: &[&str] = &[
    "-t",
    "-T",
    "-g",
    "--type",
    "--type-not",
    "--glob",
    "--iglob",
    "-A",
    "-B",
    "-C",
    "-m",
    "--max-count",
    "-j",
    "--threads",
    "-f",
    "--file",
    "--include",
    "--exclude",
    "--exclude-dir",
    "--max-depth",
    "-d",
    "-D",
    "--color",
    "--colour",
];
const SEARCH_COMMANDS: &[&str] = &[
    "grep", "egrep", "fgrep", "zgrep", "rg", "ripgrep", "find", "fd", "ack", "ag",
];
static HINT_TERM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap());
static HINT_ESCAPE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\\[^\n]").unwrap());
static HINT_SAFE_ARG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[\w.\-() ]+$").unwrap());
static HINT_UNSAFE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\x00-\x1f\x7f`]").unwrap());
static HINT_LINE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^L(\d+)").unwrap());
/// Identifier-like words in a search pattern (regex escapes and declaration
/// keywords dropped), at most three.
pub fn pattern_terms(pattern: &str) -> Vec<String> {
    let text = HINT_ESCAPE.replace_all(pattern, " ");
    HINT_TERM
        .find_iter(&text)
        .map(|m| m.as_str())
        .filter(|w| !HINT_KEYWORDS.contains(&w.to_lowercase().as_str()))
        .take(3)
        .map(str::to_owned)
        .collect()
}
/// The read target as a root-relative POSIX path, the form graph.json stores.
pub fn rel_to_root(file_path: &str, root: &Path) -> Option<String> {
    if file_path.is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(file_path.replace('\\', "/"));
    let p = if p.is_absolute() { p } else { root.join(p) };
    let resolved = crate::extractors::resolution::resolved(&p);
    let relative = resolved.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
    .map(|s| if s.is_empty() { ".".into() } else { s })
}
/// Words the agent searched for: the Grep `pattern`, or the pattern argument
/// of a shell content search. Empty when there is nothing to go on.
pub fn search_hint_terms(t: &Map<String, Value>) -> Vec<String> {
    let text = |key: &str| match t.get(key) {
        Some(v) if crate::build::truthy(v) => v
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_compat::string(v)),
        _ => String::new(),
    };
    let cmd = text("command");
    if cmd.is_empty() {
        return pattern_terms(&text("pattern"));
    }
    let Ok(tokens) = crate::detect_types::shell_split(&cmd) else {
        return vec![];
    };
    for (i, tok) in tokens.iter().enumerate() {
        let normalized = tok.replace('\\', "/");
        let name = normalized.rsplit('/').next().unwrap_or("").to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        // Content searches only: find/fd match file names, and a search fed
        // by a pipe filters that output, not the code.
        if !SEARCH_COMMANDS.contains(&name)
            || name == "find"
            || name == "fd"
            || (i > 0 && tokens[i - 1] == "|")
        {
            continue;
        }
        let mut rest = tokens[i + 1..].iter();
        while let Some(arg) = rest.next() {
            if arg == "-e" || arg == "--regexp" {
                return pattern_terms(rest.next().map_or("", String::as_str));
            }
            if HINT_VALUE_FLAGS.contains(&arg.as_str()) {
                rest.next();
                continue;
            }
            if arg.starts_with('-') {
                continue;
            }
            // The first positional argument is the pattern.
            return pattern_terms(arg);
        }
        return vec![];
    }
    vec![]
}
/// A graph field as one inert line: graph.json is repository content, so no
/// newline, control character or backtick may reach the model-facing hint.
fn hint_text(value: &Value, cap: usize) -> String {
    let raw = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_compat::string(value));
    let text = HINT_UNSAFE.replace_all(&raw, " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= cap {
        text
    } else {
        text.chars().take(cap - 1).collect::<String>() + "\u{2026}"
    }
}
/// `n.get(key) or default`.
fn or_default<'a>(node: &'a Map<String, Value>, key: &str, default: &'a Value) -> &'a Value {
    node.get(key)
        .filter(|v| crate::build::truthy(v))
        .unwrap_or(default)
}
/// (command, node lines) for a nudge: nodes whose label matches the searched
/// terms, or the symbols in the file being read. Empty for an oversized or
/// unreadable graph, or no match.
fn graph_hint(terms: &[String], rel_file: Option<&str>) -> Option<(String, Vec<String>)> {
    let path = out().join("graph.json");
    if fs::metadata(&path).ok()?.len() > HINT_GRAPH_MAX_BYTES {
        return Some((String::new(), vec![]));
    }
    let graph: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    let nodes: Vec<&Map<String, Value>> = match graph.as_object()?.get("nodes") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        _ => vec![],
    };
    let empty = json!("");
    let key = |n: &Map<String, Value>| {
        let label = or_default(n, "label", &empty);
        let label = label
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_compat::string(label))
            .to_lowercase();
        let label = label.strip_suffix("()").unwrap_or(&label);
        label.trim_start_matches('.').to_owned()
    };
    let line_of = |n: &Map<String, Value>| -> u64 {
        let loc = or_default(n, "source_location", &empty);
        let loc = loc
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_compat::string(loc));
        HINT_LINE
            .captures(&loc)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0)
    };
    let program = crate::cli::program();
    let mut command = String::new();
    let picked: Vec<&Map<String, Value>> = if let Some(rel_file) = rel_file {
        let base = rel_file.rsplit('/').next().unwrap_or(rel_file);
        let mut picked: Vec<_> = nodes
            .iter()
            .copied()
            .filter(|n| {
                n.get("source_file").and_then(Value::as_str) == Some(rel_file)
                    && n.get("label").and_then(Value::as_str) != Some(base)
            })
            .collect();
        picked.sort_by_cached_key(|n| line_of(n));
        picked.truncate(HINT_MAX_NODES);
        if !picked.is_empty() && HINT_SAFE_ARG.is_match(base) {
            command = format!("{program} explain \"{base}\"");
        }
        picked
    } else {
        let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        // Exact label matches first, then labels containing a term; shorter
        // labels first. Short terms only match exactly.
        let mut ranked: Vec<(u8, usize, &Map<String, Value>)> = nodes
            .iter()
            .copied()
            .filter_map(|n| {
                let k = key(n);
                let exact = lowered.contains(&k);
                (exact
                    || lowered
                        .iter()
                        .any(|term| term.chars().count() >= 5 && k.contains(term.as_str())))
                .then(|| (u8::from(!exact), k.chars().count(), n))
            })
            .collect();
        ranked.sort_by_key(|(rank, len, _)| (*rank, *len));
        let picked: Vec<_> = ranked
            .iter()
            .take(HINT_MAX_NODES)
            .map(|(_, _, n)| *n)
            .collect();
        if let Some((0, _, first)) = ranked.first() {
            // An exact match's label is a search term plus "." or "()": safe to quote.
            let label = first.get("label").map_or(Value::Null, Clone::clone);
            command = format!(
                "{program} explain \"{}\"",
                label
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::python_compat::string(&label))
            );
        } else if !picked.is_empty() {
            command = format!("{program} query \"{}\"", terms.join(" "));
        }
        picked
    };
    let unknown = json!("?");
    let lines = picked
        .iter()
        .map(|n| {
            let loc = hint_text(or_default(n, "source_location", &empty), 20);
            let src = hint_text(or_default(n, "source_file", &unknown), 120);
            let label = hint_text(or_default(n, "label", &empty), 80);
            if loc.is_empty() {
                format!("- {label} ({src})")
            } else {
                format!("- {label} ({src} {loc})")
            }
        })
        .collect();
    Some((command, lines))
}
/// Appends the call-specific command and the matching graph nodes to a nudge
/// or deny (0.9.89, #4368). Any failure returns the nudge unchanged: a hint
/// must never cost the nudge.
pub fn with_hint(nudge: String, terms: &[String], rel_file: Option<&str>) -> String {
    if terms.is_empty() && rel_file.is_none() {
        return nudge;
    }
    let Some((command, lines)) = graph_hint(terms, rel_file) else {
        return nudge;
    };
    if command.is_empty() && lines.is_empty() {
        return nudge;
    }
    let Ok(mut d) = serde_json::from_str::<Value>(&nudge) else {
        return nudge;
    };
    let Some(out) = d
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    else {
        return nudge;
    };
    let key = if out.contains_key("permissionDecisionReason") {
        "permissionDecisionReason"
    } else {
        "additionalContext"
    };
    let Some(text) = out.get(key).and_then(Value::as_str) else {
        return nudge;
    };
    let mut text = text.to_owned();
    if !command.is_empty() {
        text.push_str(&format!("\nStart with: `{command}`"));
    }
    if !lines.is_empty() {
        text.push_str("\nWhat the graph already has for this call:\n");
        text.push_str(&lines.join("\n"));
    }
    out.insert(key.into(), Value::String(text));
    format!("{d}\n")
}
pub fn strict(flag: bool) -> bool {
    match std::env::var("GRAPHICLE_HOOK_STRICT")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => flag,
    }
}
pub fn touch_query(graph: &Path) {
    let _ = crate::install_fs::write(
        &graph
            .parent()
            .unwrap_or(Path::new("."))
            .join("cache/last_query_stamp"),
        &SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            .to_string(),
    );
}
pub fn query_fresh() -> bool {
    let ttl = std::env::var("GRAPHICLE_HOOK_STRICT_TTL")
        .unwrap_or("1800".into())
        .parse::<f64>()
        .unwrap_or(0.0);
    out()
        .join("cache/last_query_stamp")
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| {
            SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs_f64())
                .unwrap_or(-1.0)
                < ttl
        })
}
/// A session id as a marker file name: `[^A-Za-z0-9_-]` becomes `_`, and at
/// most 64 characters are kept.
fn session_key(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
/// `str(payload.get(key) or "")`.
fn payload_text(d: &Map<String, Value>, key: &str) -> String {
    match d.get(key) {
        Some(v) if crate::build::truthy(v) => crate::python_compat::string(v),
        _ => String::new(),
    }
}
/// Best-effort removal of per-session hook markers older than a day.
fn gc_hook_session_markers(dir: &Path) {
    if let Ok(it) = fs::read_dir(dir) {
        for e in it.flatten() {
            if e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(86400))
            {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}
pub fn mark_denied(session: &str) -> bool {
    let sid = session_key(session);
    if sid.is_empty() {
        return false;
    }
    let dir = out().join("cache/hook_sessions");
    if fs::create_dir_all(&dir).is_err() {
        return false;
    }
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(format!("{sid}.denied")))
        .is_err()
    {
        return false;
    }
    gc_hook_session_markers(&dir);
    true
}
/// Whether to emit a soft read/search nudge for this tool call (Graphify
/// 0.9.87, #3435). The nudge used to be injected on every qualifying call.
/// Budgets are per agent: the session id, plus the `agent_id` Claude Code
/// sends for subagents (which share the parent's session id but start with an
/// empty context). Per agent the first nudge always fires; after that a fresh
/// query stamp means someone oriented recently, so no nudge; and at most
/// `GRAPHICLE_HOOK_NUDGE_CAP` nudges fire (default 5; 0 or a non-number
/// disables the cap), counted in a marker next to the strict-mode ones. A
/// call without a session id, or a marker error, leaves only the stamp rule.
pub fn nudge_allowed(payload: &Map<String, Value>) -> bool {
    use sha2::Digest;
    let stamp = query_fresh();
    let cap = match std::env::var("GRAPHICLE_HOOK_NUDGE_CAP") {
        Err(std::env::VarError::NotPresent) => 5,
        Ok(value) => crate::llm_config::parse_integer(&value).unwrap_or(0),
        Err(_) => 0,
    };
    let mut key = session_key(&payload_text(payload, "session_id"));
    if key.is_empty() {
        return !stamp;
    }
    let agent = payload_text(payload, "agent_id");
    if !agent.is_empty() {
        // Hashed so long ids cannot truncate two agents into one key.
        let digest = sha2::Sha256::digest(agent.as_bytes());
        key.push('-');
        key.extend(digest.iter().take(8).map(|b| format!("{b:02x}")));
    }
    let dir = out().join("cache/hook_sessions");
    if fs::create_dir_all(&dir).is_err() {
        return !stamp;
    }
    let marker = dir.join(format!("{key}.nudges"));
    let count = fs::read_to_string(&marker)
        .ok()
        .map(|s| {
            let s = s.trim_matches(crate::dedup_strings::whitespace);
            if s.is_empty() {
                Some(0)
            } else {
                crate::llm_config::parse_integer(s)
            }
        })
        .and_then(|n| n)
        .unwrap_or(0);
    if count != 0 && stamp {
        return false;
    }
    if 0 < cap && cap <= count {
        return false;
    }
    if crate::paths::write_text_atomic(&marker, &(count.saturating_add(1)).to_string()).is_err() {
        return !stamp;
    }
    if count == 0 {
        gc_hook_session_markers(&dir);
    }
    true
}
pub fn bash_search(cmd: &str) -> bool {
    let heredoc = fancy_regex::Regex::new(r#"<<-?\s*(['\"]?)(\w+)\1"#).unwrap();
    let mut text = cmd.to_string();
    let mut pos = 0;
    while let Ok(Some(cap)) = heredoc.captures_from_pos(&text, pos) {
        let Some(nl) = text[cap.get(0).unwrap().end()..]
            .find('\n')
            .map(|n| n + cap.get(0).unwrap().end())
        else {
            break;
        };
        let term = regex::Regex::new(&format!(
            r"(?m)^\s*{}\s*$",
            regex::escape(cap.get(2).unwrap().as_str())
        ))
        .unwrap();
        let tail = term.find_at(&text, nl + 1).map(|m| m.end());
        text = if let Some(end) = tail {
            format!("{}{}", &text[..nl + 1], &text[end..])
        } else {
            text[..nl + 1].into()
        };
        pos = nl + 1;
        if tail.is_none() {
            break;
        }
    }
    // A quoted span becomes one placeholder word: never the executable, but
    // a quoted sed/awk program is still an operand (Graphify 0.9.101).
    text = regex::Regex::new(r"'[^']*'")
        .unwrap()
        .replace_all(&text, "Q")
        .into_owned();
    text = regex::Regex::new(r#""[^"]*""#)
        .unwrap()
        .replace_all(&text, "Q")
        .into_owned();
    // Command segments split at shell operators, each knowing whether it
    // reads a pipe; a pipe carries over the empty segments of `|&` and of a
    // pipeline continued on the next line.
    let operators = regex::Regex::new(r"\|\|?|[;&\n]|\$\(|`|\(|\)|\{|\}").unwrap();
    let mut segments: Vec<(Option<&str>, &str)> = Vec::new();
    let mut start = 0;
    let mut before = None;
    for m in operators.find_iter(&text) {
        segments.push((before, &text[start..m.start()]));
        before = Some(m.as_str());
        start = m.end();
    }
    segments.push((before, &text[start..]));
    let mut piped = false;
    for (operator, segment) in segments {
        if let Some(op) = operator
            && (op == "|" || op == "||")
        {
            piped = op == "|";
        }
        let tokens = segment.split_whitespace().collect::<Vec<_>>();
        if tokens.is_empty() {
            continue;
        }
        let segment_piped = std::mem::replace(&mut piped, false);
        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i];
            if tok.rsplit('/').next().unwrap_or("").contains('=') && !tok.starts_with(['-', '/']) {
                i += 1;
                continue;
            }
            let normalized = tok.replace('\\', "/").to_lowercase();
            let name = normalized.rsplit('/').next().unwrap_or("");
            let name = name.strip_suffix(".exe").unwrap_or(name);
            if [
                "sudo", "command", "exec", "nohup", "time", "nice", "ionice", "env", "xargs",
                "timeout", "stdbuf", "doas",
            ]
            .contains(&name)
            {
                i += 1;
                while i < tokens.len() && tokens[i].starts_with('-') {
                    i += 1;
                }
                continue;
            }
            if [
                "grep", "egrep", "fgrep", "zgrep", "rg", "ripgrep", "find", "fd", "ack", "ag",
            ]
            .contains(&name)
            {
                return true;
            }
            if name == "git" && tokens[i + 1..(i + 4).min(tokens.len())].contains(&"grep") {
                return true;
            }
            // sed and awk count when they open a file themselves: not
            // reading a pipe, not editing in place, and a program plus at
            // least one file (`x=1` is an awk assignment) (Graphify 0.9.101).
            if (name == "sed" || name == "awk") && !segment_piped {
                let rest = &tokens[i + 1..];
                let in_place = rest.iter().any(|t2| {
                    t2.starts_with("--in-place")
                        || if name == "awk" {
                            t2.starts_with("-i")
                        } else {
                            t2.starts_with('-') && !t2.starts_with("--") && t2.contains('i')
                        }
                });
                let operands = rest
                    .iter()
                    .filter(|t2| !t2.starts_with('-') && !t2.contains('='))
                    .count();
                if !in_place && operands >= 2 {
                    return true;
                }
            }
            break;
        }
    }
    false
}
pub fn cwd_relative(path: &str, windows: bool) -> bool {
    if windows {
        !path.starts_with(['\\', '/']) && (path.as_bytes().get(1) != Some(&b':'))
    } else {
        !path.starts_with('/')
    }
}
pub fn target_indexed(file: &str, root: &Path) -> bool {
    if file.is_empty() {
        return true;
    }
    let path = out().join("manifest.json");
    let Ok(meta) = path.metadata() else {
        return true;
    };
    if meta.len() > 2_000_000 {
        return true;
    }
    let Ok(data) = crate::install_fs::settings(&path) else {
        return true;
    };
    let Some(keys) = data.as_object().filter(|m| !m.is_empty()) else {
        return true;
    };
    let p = Path::new(file);
    let mut rels = vec![
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    ];
    if let Ok(rel) = crate::extractors::resolution::resolved(p).strip_prefix(root) {
        rels.push(rel.to_string_lossy().replace('\\', "/"));
    }
    let abs = file.replace('\\', "/");
    keys.keys().map(|k| k.replace('\\', "/")).any(|k| {
        k == abs
            || rels
                .iter()
                .any(|r| !r.is_empty() && (k == *r || k.ends_with(&format!("/{r}"))))
    })
}
/// Whether a hook target resolves inside the output directory: the configured
/// one or the default `.graphicle` under `root` (#3959). Containment, not a
/// substring, so `tools/graphicle/...` is not skipped when
/// `GRAPHICLE_OUT=artifacts/graphicle`.
pub fn under_out(target: &str, root: &Path, configured: &str, windows: bool) -> bool {
    if target.is_empty() {
        return false;
    }
    let resolve = |p: &str| {
        let p = p.replace('\\', "/");
        let path = if cwd_relative(&p, windows) {
            root.join(&p)
        } else {
            std::path::PathBuf::from(p)
        };
        crate::extractors::resolution::resolved(&path)
    };
    let mut dirs = vec![resolve(configured)];
    let default =
        crate::extractors::resolution::resolved(&root.join(crate::paths::DEFAULT_OUT_DIR));
    if !dirs.contains(&default) {
        dirs.push(default);
    }
    let lower = |p: &Path| std::path::PathBuf::from(p.to_string_lossy().to_lowercase());
    let target = resolve(target);
    dirs.iter()
        .any(|dir| target.starts_with(dir) || lower(&target).starts_with(lower(dir)))
}
/// `_target_matches_manifest` (Graphify 0.9.102): a file newer than
/// graph.json still matches its saved extraction. The manifest (read up to
/// 2,000,000 bytes) must list the exact project-relative path, or the exact
/// absolute one, and every populated AST/semantic hash must equal the
/// file's MD5; any doubt keeps the file stale.
pub fn target_matches_manifest(file_path: &str, root: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = fs::File::open(out().join("manifest.json")) else {
        return false;
    };
    let mut raw = Vec::new();
    if file.take(2_000_001).read_to_end(&mut raw).is_err() || raw.len() > 2_000_000 {
        return false;
    }
    let Ok(Value::Object(manifest)) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let p = crate::detect_ignore::absolute_lexical(Path::new(file_path));
    let Ok(rel) = p.strip_prefix(root) else {
        return false;
    };
    let rel = crate::paths::nfc(&rel.to_string_lossy().replace('\\', "/"));
    let Some(Value::Object(entry)) = manifest
        .get(&rel)
        .or_else(|| manifest.get(&crate::paths::nfc(&p.to_string_lossy())))
    else {
        return false;
    };
    let hashes: Vec<&Value> = if entry.contains_key("hash") && !entry.contains_key("ast_hash") {
        vec![&entry["hash"]]
    } else {
        [entry.get("ast_hash"), entry.get("semantic_hash")]
            .into_iter()
            .map(|h| h.unwrap_or(&EMPTY))
            .collect()
    };
    if !hashes.iter().all(|h| h.is_string()) {
        return false;
    }
    let hashes: std::collections::BTreeSet<&str> = hashes
        .iter()
        .filter_map(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .collect();
    if hashes.len() != 1 {
        return false;
    }
    hashes.into_iter().next() == Some(crate::detect_manifest::md5_file(&p).as_str())
}
static EMPTY: Value = Value::String(String::new());
/// `_search_is_outside_project` (Graphify 0.9.102, #4961): every literal
/// target of a structured Grep, or of a simple `grep`/`rg` command, is
/// outside the project. Shell composition, expansion, wrappers and unknown
/// options stay advisory: this is not a shell interpreter or an access gate.
pub fn search_is_outside_project(t: &Map<String, Value>) -> bool {
    let command = t
        .get("command")
        .filter(|v| crate::build::truthy(v))
        .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| crate::python_compat::string(v)))
        .unwrap_or_default();
    let paths: Vec<String> = if !command.is_empty() {
        if command.chars().any(|c| "$`;&|<>\n\\".contains(c)) {
            return false;
        }
        let Ok(tokens) = crate::detect_types::shell_split(&command) else {
            return false;
        };
        let Some(first) = tokens.first() else {
            return false;
        };
        if !["grep", "egrep", "fgrep", "zgrep", "rg", "ripgrep"]
            .contains(&first.rsplit('/').next().unwrap_or(""))
        {
            return false;
        }
        let short = regex::Regex::new(r"^-[nriIvVwxlLchHqsoEFGPRUa]+$").unwrap();
        let mut positional = vec![];
        let mut explicit_pattern = false;
        let mut options = true;
        let mut args = tokens[1..].iter();
        while let Some(arg) = args.next() {
            if options && arg == "--" {
                options = false;
            } else if options && ["-e", "--regexp", "-f", "--file"].contains(&arg.as_str()) {
                if args.next().is_none() {
                    return false;
                }
                explicit_pattern = true;
            } else if options && (arg.starts_with("--regexp=") || arg.starts_with("--file=")) {
                explicit_pattern = true;
            } else if options && HINT_VALUE_FLAGS.contains(&arg.as_str()) {
                if args.next().is_none() {
                    return false;
                }
            } else if options && arg.starts_with("--") {
                let name = arg.split('=').next().unwrap_or("");
                if HINT_VALUE_FLAGS.contains(&name) && arg.contains('=') {
                    continue;
                }
                if ![
                    "--ignore-case",
                    "--line-number",
                    "--recursive",
                    "--fixed-strings",
                    "--word-regexp",
                    "--invert-match",
                ]
                .contains(&arg.as_str())
                {
                    return false;
                }
            } else if options && arg.starts_with('-') {
                if !short.is_match(arg) {
                    return false;
                }
            } else {
                positional.push(arg.clone());
            }
        }
        if explicit_pattern {
            positional
        } else {
            positional.into_iter().skip(1).collect()
        }
    } else {
        vec![
            t.get("path")
                .filter(|v| crate::build::truthy(v))
                .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| crate::python_compat::string(v)))
                .unwrap_or_default(),
        ]
    };
    let glob = regex::Regex::new(r"[$`*?\[\]{}~]").unwrap();
    if paths.is_empty() || paths.iter().any(|p| p.is_empty() || p == "-" || glob.is_match(p)) {
        return false;
    }
    // Bash on Windows may use /c/... or /mnt/c/... paths whose drive is
    // unknown here; the hint is kept.
    if !command.is_empty() && cfg!(windows) && paths.iter().any(|p| p.starts_with('/')) {
        return false;
    }
    let root = crate::extractors::resolution::resolved(Path::new(
        &std::env::var("CLAUDE_PROJECT_DIR").unwrap_or_else(|_| ".".into()),
    ));
    paths
        .iter()
        .all(|p| !crate::extractors::resolution::resolved(Path::new(p)).starts_with(&root))
}
pub fn run(kind: &str, data: &Value, strict_flag: bool) -> String {
    let graph = out().join("graph.json");
    if kind == "gemini" {
        return if graph.is_file() {
            json!({"decision":"allow","additionalContext":crate::cli::program_text(DATA["_GEMINI_NUDGE_TEXT"].as_str().unwrap_or(""))}).to_string()
        } else {
            json!({"decision":"allow"}).to_string()
        };
    }
    let Some(d) = data.as_object() else {
        return String::new();
    };
    let input = d.get("tool_input").unwrap_or(data);
    let Some(t) = input.as_object() else {
        return String::new();
    };
    let s = |key| {
        t.get(key)
            .filter(|v| !v.is_null())
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.into()
                } else {
                    crate::python_compat::string(v)
                }
            })
            .unwrap_or_default()
    };
    let emit = |name| crate::cli::program_text(DATA[name].as_str().unwrap_or("")).into_owned();
    if kind == "search" {
        let cmd = s("command");
        // A dedicated Grep is scoped to the project's source targets and,
        // in strict mode, shares the once-per-session block with Read
        // (Graphify 0.9.102). Bash searches stay advisory: a compound shell
        // command has no single reliable target.
        let is_grep_tool = cmd.is_empty()
            && t.get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
            && d.get("tool_name")
                .is_none_or(|v| v.is_null() || v == "Grep");
        let is_bash_search = !cmd.is_empty() && bash_search(&cmd);
        if !(is_grep_tool || is_bash_search) || !graph.is_file() {
            return String::new();
        }
        if is_grep_tool {
            let path = match t.get("path") {
                None => ".".to_string(),
                Some(Value::String(p)) => p.clone(),
                Some(_) => return String::new(),
            };
            let root = crate::extractors::resolution::resolved(Path::new(
                &std::env::var("CLAUDE_PROJECT_DIR").unwrap_or_else(|_| ".".into()),
            ));
            let mut target = std::path::PathBuf::from(path.replace('\\', "/"));
            if cwd_relative(&path, cfg!(windows)) {
                target = root.join(target);
            }
            let target = crate::extractors::resolution::resolved(&target);
            if !target.starts_with(&root) {
                return String::new();
            }
            for output in [out(), root.join(crate::paths::DEFAULT_OUT_DIR)] {
                let output = if cwd_relative(&output.to_string_lossy(), cfg!(windows)) {
                    root.join(output)
                } else {
                    output
                };
                if target.starts_with(crate::extractors::resolution::resolved(&output)) {
                    return String::new();
                }
            }
            // A single-file target needs the same index evidence as Read; a
            // folder is the code the graph covers.
            let indexed =
                !target.is_file() || target_indexed(&target.to_string_lossy(), &root);
            if indexed
                && strict(strict_flag)
                && !out().join("needs_update").exists()
                && !query_fresh()
                && mark_denied(&payload_text(d, "session_id"))
            {
                return with_hint(emit("_SEARCH_DENY"), &search_hint_terms(t), None);
            }
        }
        // A search whose every literal target is outside the project gets
        // no project hint (Graphify 0.9.102, #4961).
        if !search_is_outside_project(t) && nudge_allowed(d) {
            return with_hint(
                nudge_for_out(emit("_SEARCH_NUDGE")),
                &search_hint_terms(t),
                None,
            );
        }
        return String::new();
    }
    if kind != "read" {
        return String::new();
    }
    let vals = [s("file_path"), s("pattern"), s("path")];
    if !vals.iter().any(|v| {
        let value = v.to_lowercase().replace('\\', "/");
        let name = value.rsplit('/').next().unwrap_or("");
        name.rsplit_once('.').is_some_and(|(_, s)| {
            DATA["_HOOK_SOURCE_EXTS"]
                .as_array()
                .unwrap()
                .contains(&json!(format!(".{s}")))
        })
    }) {
        return String::new();
    }
    let root = crate::extractors::resolution::resolved(Path::new(
        &std::env::var("CLAUDE_PROJECT_DIR").unwrap_or_else(|_| ".".into()),
    ));
    let mut candidates = vec![vals[0].clone(), vals[2].clone(), vals[1].clone()];
    if !vals[2].is_empty() && !vals[1].is_empty() {
        candidates.push(format!("{}/{}", vals[2], vals[1]));
    }
    if candidates
        .iter()
        .any(|c| under_out(c, &root, crate::paths::OUT_DIR.as_str(), cfg!(windows)))
    {
        return String::new();
    }
    let explicit = [&vals[0], &vals[2]]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if !explicit.is_empty()
        && !explicit.iter().any(|s| {
            cwd_relative(s, cfg!(windows))
                || crate::extractors::resolution::resolved(Path::new(s)).starts_with(&root)
        })
    {
        return String::new();
    }
    let Ok(gtime) = graph.metadata().and_then(|m| m.modified()) else {
        return String::new();
    };
    // A checkout can refresh modification times without changing what was
    // extracted: a newer file whose content still matches its manifest
    // hashes is not stale (Graphify 0.9.102). The `needs_update` marker
    // always softens.
    let stale = out().join("needs_update").exists()
        || (!vals[0].is_empty()
            && fs::metadata(&vals[0])
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime > gtime)
            && !target_matches_manifest(&vals[0], &root));
    if stale {
        return if nudge_allowed(d) {
            nudge_for_out(emit("_READ_NUDGE_STALE"))
        } else {
            String::new()
        };
    }
    if strict(strict_flag)
        && d.get("tool_name")
            .is_none_or(|v| v.is_null() || v == "Read")
        && !query_fresh()
        && target_indexed(&vals[0], &root)
        && mark_denied(&payload_text(d, "session_id"))
    {
        let rel = rel_to_root(&vals[0], &root);
        return with_hint(emit("_READ_DENY"), &[], rel.as_deref());
    }
    if nudge_allowed(d) {
        let rel = rel_to_root(&vals[0], &root);
        with_hint(nudge_for_out(emit("_READ_NUDGE")), &[], rel.as_deref())
    } else {
        String::new()
    }
}
#[cfg(test)]
mod tests {
    use super::under_out;

    #[test]
    fn hook_nudges_name_the_effective_graph_json() {
        let nudge = super::DATA["_READ_NUDGE"].as_str().unwrap().to_string();
        for default in [".graphicle", "./.graphicle", ".graphicle/"] {
            assert_eq!(super::nudge_for_out_with(nudge.clone(), default), nudge);
        }
        let custom = super::nudge_for_out_with(nudge.clone(), "artifacts/graphicle");
        assert!(custom.ends_with("}}\n"), "{custom}");
        let d: serde_json::Value = serde_json::from_str(&custom).unwrap();
        let context = d["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            context.contains("knowledge graph at artifacts/graphicle/graph.json"),
            "{context}"
        );
        assert!(!context.contains(".graphicle/graph.json"), "{context}");
        assert_eq!(d["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        let stale = super::DATA["_READ_NUDGE_STALE"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(super::nudge_for_out_with(stale, "/shared/out").contains("/shared/out/graph.json"));
    }

    /// tests/test_search_hook_scope.py (Graphify 0.9.102, #4961): a search
    /// whose every literal target is outside the project gets no hint;
    /// composition, globs and unknown options stay advisory.
    #[cfg(unix)]
    #[test]
    fn searches_outside_the_project_get_no_hint() {
        if std::env::var_os("CLAUDE_PROJECT_DIR").is_some() {
            return;
        }
        let input = |v: serde_json::Value| v.as_object().unwrap().clone();
        let outside = super::search_is_outside_project;
        assert!(outside(&input(serde_json::json!({"command": "grep -rn foo /"}))));
        assert!(outside(&input(serde_json::json!({"command": "rg -e foo /"}))));
        assert!(outside(&input(serde_json::json!({"pattern": "foo", "path": "/"}))));
        for inside in [
            "grep -rn foo src",
            "grep -rn foo / | head",
            "grep -rn foo /*",
            "grep --context=3 foo",
            "grep --weird foo /",
            "find / -name x",
        ] {
            assert!(!outside(&input(serde_json::json!({"command": inside}))), "{inside}");
        }
        assert!(!outside(&input(serde_json::json!({"pattern": "foo"}))));
    }

    /// tests/test_hook_guard_token_match.py (Graphify 0.9.101): sed and awk
    /// that open files count as searches; on a pipe, in place or with no
    /// file they stay quiet.
    #[test]
    fn sed_and_awk_reading_files_count_as_searches() {
        for cmd in [
            "sed -n 4085,4145p src/inc.php",
            "awk 'NR>=10 && /warn/ {print NR}' src/a.py",
            "sed -n '/def x/,/^$/p' a.py | head",
            "cd src && awk -F: '{print $1}' a.py",
        ] {
            assert!(super::bash_search(cmd), "{cmd}");
        }
        for cmd in [
            "ps aux | awk '{print $2}'",
            "git log --oneline | sed -n 1,5p",
            "echo x | sed s/a/b/",
            "sed -i 's/a/b/' f.txt",
            "gh pr list | awk -F'\\t' '{print $1}'",
            "sed -Ei 's/a/b/' f.txt",
            "sed -ni 's/a/b/p' f.txt",
            "git log |\n  awk '{print $1}'",
            "make 2>&1 |& sed -n 1,5p",
            "awk 'BEGIN { print 1 }'",
            "sed -n 1,5p",
        ] {
            assert!(!super::bash_search(cmd), "{cmd}");
        }
    }

    #[test]
    fn hook_reminders_state_facts_not_commands() {
        // test_hook_nudge_wording.py (Graphify 0.9.96, #2202): commands in a
        // tool result read like a prompt injection.
        let commanding = |text: &str| {
            let lower = text.to_lowercase();
            lower.contains("mandatory")
                || lower.contains("subagent prompt")
                || text
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|w| w.eq_ignore_ascii_case("must"))
        };
        for name in ["_SEARCH_NUDGE", "_READ_NUDGE", "_READ_NUDGE_STALE"] {
            let d: serde_json::Value =
                serde_json::from_str(super::DATA[name].as_str().unwrap()).unwrap();
            let text = d["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap();
            assert!(!commanding(text), "{name}: {text}");
        }
        let d: serde_json::Value =
            serde_json::from_str(super::DATA["_READ_DENY"].as_str().unwrap()).unwrap();
        let reason = d["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(!reason.to_lowercase().contains("subagent"), "{reason}");
    }

    #[test]
    fn nested_output_dir_is_matched_by_containment_not_substring() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::extractors::resolution::resolved(dir.path());
        let out = "artifacts/graphicle";
        for outside in [
            "tools/graphicle/main.py",
            ".codex/skills/graphicle/helper.py",
            "src/graphicle/x.py",
        ] {
            assert!(!under_out(outside, &root, out, false), "{outside}");
        }
        for inside in [
            "artifacts/graphicle/GRAPH_REPORT.md",
            "artifacts/graphicle/report.py",
            "artifacts/graphicle/**/*.py",
            "artifacts/graphicle/*.py",
            "artifacts/graphicle",
            ".graphicle/graph.json",
        ] {
            assert!(under_out(inside, &root, out, false), "{inside}");
        }
        let absolute = root.join("artifacts/graphicle/report.py");
        assert!(under_out(&absolute.to_string_lossy(), &root, out, false));
    }
}
