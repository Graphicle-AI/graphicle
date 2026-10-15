// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/querylog.py
//! Port of graphify/querylog.py: opt-in, append-only, fail-silent logging.
use crate::{Result, decimal_data, fsutil, regex_compat};
use chrono::{SecondsFormat, Timelike, Utc};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub const ENV_KEYS: &[&str] = &[
    "GRAPHICLE_QUERY_LOG",
    "GRAPHICLE_QUERY_LOG_ENABLE",
    "GRAPHICLE_QUERY_LOG_DISABLE",
    "GRAPHICLE_QUERY_LOG_RESPONSES",
];
static NODES: LazyLock<Regex> = LazyLock::new(|| {
    let digits = format!(
        "[{}]",
        decimal_data::DECIMAL
            .iter()
            .map(|(start, end)| format!(r"\u{{{start:x}}}-\u{{{end:x}}}"))
            .collect::<String>()
    );
    regex_compat::compile(&format!(r"({digits}+)\s+nodes?\s+found"))
});
pub fn nodes_from_result(result: &str) -> Option<u64> {
    let c = NODES.captures(result)?;
    c[1].chars().try_fold(0u64, |value, c| {
        let point = c as u32;
        let range = decimal_data::DECIMAL.partition_point(|(start, _)| *start <= point) - 1;
        value
            .checked_mul(10)?
            .checked_add(((point - decimal_data::DECIMAL[range].0) % 10) as u64)
    })
}
fn enabled(value: Option<&String>) -> bool {
    value.is_some_and(|s| ["1", "true", "yes"].contains(&s.to_lowercase().as_str()))
}
pub fn log_responses(env: &HashMap<String, String>) -> bool {
    enabled(env.get("GRAPHICLE_QUERY_LOG_RESPONSES"))
}
fn expanduser(path: &str, home: &Path) -> Result<PathBuf> {
    if path == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(tail) = path.strip_prefix("~/") {
        return Ok(home.join(tail));
    }
    if let Some(rest) = path.strip_prefix('~') {
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        #[cfg(unix)]
        {
            let user = nix::unistd::User::from_name(name)
                .map_err(crate::Error::from)?
                .ok_or("Could not determine home directory")?;
            return Ok(user.dir.join(tail));
        }
        #[cfg(not(unix))]
        {
            let _ = (name, tail);
            return Err("Named home expansion is unavailable on this platform".into());
        }
    }
    Ok(PathBuf::from(path))
}
pub fn log_path(env: &HashMap<String, String>, home: &Path) -> Result<Option<PathBuf>> {
    if enabled(env.get("GRAPHICLE_QUERY_LOG_DISABLE")) {
        return Ok(None);
    }
    if let Some(value) = env.get("GRAPHICLE_QUERY_LOG") {
        let value =
            value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
        if !value.is_empty() {
            return expanduser(value, home).map(Some);
        }
    }
    Ok(enabled(env.get("GRAPHICLE_QUERY_LOG_ENABLE"))
        .then(|| home.join(".cache/graphicle/queries.log")))
}
pub fn current_environment() -> HashMap<String, String> {
    ENV_KEYS
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).into(), value)))
        .collect()
}
fn home_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }
    #[cfg(unix)]
    {
        nix::unistd::User::from_uid(nix::unistd::Uid::current())
            .map_err(crate::Error::from)?
            .map(|user| user.dir)
            .ok_or_else(|| "Could not determine home directory".into())
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .ok_or_else(|| "Could not determine home directory".into())
    }
}
pub fn record(parameters: &Value, timestamp: &str, responses: bool) -> Result<Value> {
    let result = parameters.get("result").filter(|v| !v.is_null());
    let mut nodes = parameters
        .get("nodes_returned")
        .cloned()
        .unwrap_or(Value::Null);
    if nodes.is_null()
        && let Some(result) = result
    {
        nodes = json!(nodes_from_result(
            result.as_str().ok_or("Expected result text")?
        ));
    }
    let mut record = json!({"ts":timestamp,"kind":parameters["kind"],"question":parameters["question"],"corpus":parameters["corpus"],"nodes_returned":nodes});
    if let Some(result) = result {
        record["result_chars"] = json!(
            result
                .as_str()
                .ok_or("Expected result text")?
                .chars()
                .count()
        );
    }
    if let Some(duration) = parameters.get("duration_ms").filter(|v| !v.is_null()) {
        let rounded = format!(
            "{:.3}",
            duration.as_f64().ok_or("Expected numeric duration")?
        )
        .parse::<f64>()
        .map_err(crate::Error::from)?;
        record["duration_ms"] = json!(rounded);
    }
    if let Some(extra) = parameters["extra"].as_object() {
        for (key, value) in extra {
            if !value.is_null() {
                record[key] = value.clone();
            }
        }
    }
    if let Some(result) = result
        && responses
    {
        record["response"] = result.clone();
    }
    Ok(record)
}
pub fn write_with(parameters: &Value, env: &HashMap<String, String>, home: &Path, timestamp: &str) {
    let _ = (|| -> Result<()> {
        let Some(path) = log_path(env, home)? else {
            return Ok(());
        };
        let record = record(parameters, timestamp, log_responses(env))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(crate::Error::from)?;
        let mut data = serde_json::to_vec(&record).map_err(crate::Error::from)?;
        data.push(b'\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(&data))
            .map_err(crate::Error::from)
    })();
}
pub fn log_query(parameters: &Value) {
    if let Ok(home) = home_dir() {
        let now = Utc::now();
        let timestamp = now.to_rfc3339_opts(
            if now.nanosecond() / 1000 == 0 {
                SecondsFormat::Secs
            } else {
                SecondsFormat::Micros
            },
            false,
        );
        write_with(parameters, &current_environment(), &home, &timestamp);
    }
}

pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "querylog.nodes" => Ok(json!(nodes_from_result(
            request["text"].as_str().unwrap_or("")
        ))),
        "querylog.record" => record(
            &request["parameters"],
            request["timestamp"].as_str().ok_or("Expected timestamp")?,
            request["responses"].as_bool().unwrap_or(false),
        ),
        operation => {
            let env: HashMap<String, String> =
                serde_json::from_value(request.get("env").cloned().unwrap_or(json!({})))
                    .map_err(|_| "Expected log environment")?;
            let home = Path::new(request["home"].as_str().ok_or("Expected home directory")?);
            if operation == "querylog.settings" {
                Ok(
                    json!({"path":log_path(&env,home)?.map(|p|fsutil::path_string(&p)),"responses":log_responses(&env)}),
                )
            } else if operation == "querylog.write" {
                write_with(
                    &request["parameters"],
                    &env,
                    home,
                    request["timestamp"].as_str().ok_or("Expected timestamp")?,
                );
                Ok(Value::Null)
            } else {
                Err("Unknown querylog operation".into())
            }
        }
    }
}
