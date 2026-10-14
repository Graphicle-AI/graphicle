// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
    text = regex::Regex::new(r"'[^']*'")
        .unwrap()
        .replace_all(&text, " ")
        .into_owned();
    text = regex::Regex::new(r#""[^"]*""#)
        .unwrap()
        .replace_all(&text, " ")
        .into_owned();
    for segment in regex::Regex::new(r"[|;&\n]|\$\(|`|\(|\)|\{|\}")
        .unwrap()
        .split(&text)
    {
        let tokens = segment.split_whitespace().collect::<Vec<_>>();
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
        if ((cmd.is_empty() && t.get("pattern").is_some_and(crate::build::truthy))
            || (!cmd.is_empty() && bash_search(&cmd)))
            && graph.is_file()
            && nudge_allowed(d)
        {
            return nudge_for_out(emit("_SEARCH_NUDGE"));
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
    let stale = out().join("needs_update").exists()
        || (!vals[0].is_empty()
            && fs::metadata(&vals[0])
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime > gtime));
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
        return emit("_READ_DENY");
    }
    if nudge_allowed(d) {
        nudge_for_out(emit("_READ_NUDGE"))
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
            context.contains("artifacts/graphicle/graph.json exists"),
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
