// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Structured response recovery and partial-result bookkeeping (llm.py).
use crate::{
    Result,
    build::Diagnostics,
    exporters::common::truth,
    file_slice::{Unit, unit_path},
    python_compat::{repr_string, string},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::LazyLock};
pub const KEYS: [&str; 3] = ["nodes", "edges", "hyperedges"];
pub const MAX_JSON: usize = 10 * 1024 * 1024;
pub fn empty() -> Value {
    json!({"nodes":[],"edges":[],"hyperedges":[]})
}
pub fn sanitize(mut v: Value, d: &mut Diagnostics) -> Value {
    let Some(m) = v.as_object_mut() else {
        return empty();
    };
    for key in KEYS {
        if let Some(a) = m.get_mut(key).filter(|v| !v.is_null()) {
            if let Some(a) = a.as_array_mut() {
                a.retain(Value::is_object)
            } else {
                *a = json!([])
            }
        }
    }
    if let Some(hs) = m.get_mut("hyperedges").and_then(Value::as_array_mut) {
        for he in hs {
            if he["nodes"].is_array() {
                // The llm.py choke point only coerces the nodes list. Preserve any
                // alternate keys here; full build normalization owns their removal.
                let mut temporary = json!({"id":he.get("id").cloned().unwrap_or(json!("?")),"nodes":he["nodes"].clone()});
                d.stderr
                    .extend(crate::hyperedge_members::normalize(&mut temporary));
                he["nodes"] = temporary["nodes"].take();
            }
        }
    }
    v
}
pub fn balanced_object(text: &str, start: usize) -> Option<String> {
    let chars: Vec<_> = text.chars().collect();
    let mut depth = 0_i64;
    let mut quoted = false;
    let mut escape = false;
    for i in start..chars.len() {
        let c = chars[i];
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        if c == '{' {
            depth += 1
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(chars[start..=i].iter().collect());
            }
        }
    }
    None
}
pub fn object_candidates(text: &str) -> Vec<usize> {
    let chars: Vec<_> = text.chars().collect();
    let mut preferred = vec![];
    let mut rest = vec![];
    for (i, c) in chars.iter().enumerate() {
        if *c != '{' {
            continue;
        }
        let window: String = chars[i..(i + 200).min(chars.len())].iter().collect();
        let bucket = if KEYS.iter().any(|k| window.contains(&format!("\"{k}\""))) {
            &mut preferred
        } else {
            &mut rest
        };
        if bucket.len() < 64 {
            bucket.push(i)
        } else if preferred.len() >= 64 && rest.len() >= 64 {
            break;
        }
    }
    preferred.extend(rest);
    preferred
}
pub fn fragment_candidates(text: &str) -> Vec<String> {
    static FENCES: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?s)```[ \t]*([A-Za-z0-9_+-]*)[ \t]*\r?\n(.*?)```").unwrap()
    });
    let mut fences: Vec<_> = FENCES
        .captures_iter(text)
        .map(|c| {
            (
                !["json", ""].contains(&c[1].to_lowercase().as_str()),
                crate::llm_config::trim(&c[2]).to_owned(),
            )
        })
        .collect();
    fences.sort_by_key(|c| c.0);
    let mut out: Vec<_> = fences.into_iter().map(|c| c.1).collect();
    out.extend(
        object_candidates(text)
            .into_iter()
            .filter_map(|i| balanced_object(text, i)),
    );
    out
}
pub fn parse(raw: &str, d: &mut Diagnostics) -> Value {
    if raw.chars().count() > MAX_JSON {
        d.stderr.push(format!("[graphicle] LLM response exceeds {MAX_JSON} bytes ({} bytes); refusing to parse and dropping chunk.",raw.chars().count()));
        return empty();
    }
    static THINK: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?is)<(think|thinking|reasoning)>.*?</\1>").unwrap()
    });
    let stripped = THINK.replace_all(raw, " ");
    let stripped = crate::llm_config::trim(&stripped);
    if let Ok(p) = serde_json::from_str::<Value>(stripped)
        && p.is_object()
    {
        return sanitize(p, d);
    }
    let mut empty_fragment = None;
    let mut fallback = None;
    for candidate in fragment_candidates(stripped) {
        let Ok(p) = serde_json::from_str::<Value>(&candidate) else {
            continue;
        };
        let Some(m) = p.as_object() else { continue };
        if KEYS.iter().any(|k| m.contains_key(*k)) {
            let p = sanitize(p, d);
            if KEYS.iter().any(|k| truth(&p[*k])) {
                return p;
            }
            if empty_fragment.is_none() {
                empty_fragment = Some(p)
            }
        } else if fallback.is_none() {
            fallback = Some(p)
        }
    }
    if let Some(p) = empty_fragment.or(fallback) {
        return sanitize(p, d);
    }
    d.stderr.push(format!(
        "[graphicle] LLM returned invalid JSON, skipping chunk (first 200 chars: {})",
        repr_string(&raw.chars().take(200).collect::<String>())
    ));
    empty()
}
pub fn anthropic_text<'a>(content: &'a Value, default: Option<&'a str>) -> Option<&'a str> {
    content
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|b| {
                if b.get("type")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some("text"))
                {
                    return None;
                }
                b["text"]
                    .as_str()
                    .filter(|s| !crate::llm_config::trim(s).is_empty())
            })
        })
        .or(default)
}
pub fn bedrock_text<'a>(resp: &'a Value, default: &'a str) -> &'a str {
    resp["output"]["message"]["content"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|b| {
                b["text"]
                    .as_str()
                    .filter(|s| !crate::llm_config::trim(s).is_empty())
            })
        })
        .unwrap_or(default)
}
pub fn hollow(raw: Option<&str>, v: &Value) -> bool {
    raw.is_none_or(|s| crate::llm_config::trim(s).is_empty()) || !KEYS.iter().any(|k| truth(&v[*k]))
}
pub fn mark_hollow(v: &mut Value, raw: Option<&str>, backend: &str, d: &mut Diagnostics) {
    if v["finish_reason"].as_str() != Some("length") && hollow(raw, v) {
        d.stderr.push(format!("[graphicle] {} returned a hollow response (content={}, output_tokens={}); will retry the same chunk (a hollow response is not a size problem, so the chunk is not bisected).",if backend.is_empty(){"backend"}else{backend},if raw.is_none_or(|s|crate::llm_config::trim(s).is_empty()){"empty"}else{"no nodes/edges"},string(v.get("output_tokens").unwrap_or(&json!(0)))));
        v["finish_reason"] = json!("hollow");
    }
}
pub fn envelope_after_preamble(stdout: &str) -> Option<Value> {
    for (i, c) in stdout.char_indices() {
        if c != '{' && c != '[' {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&stdout[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next()
            && (v.is_object() || v.is_array())
        {
            return Some(v);
        }
    }
    None
}
pub fn cli_envelope(stdout: &str) -> Result<Value> {
    let value = serde_json::from_str(stdout)
        .or_else(|e| envelope_after_preamble(stdout).ok_or(e))
        .map_err(|e| {
            format!(
                "claude -p produced unparseable JSON envelope: {e}; first 500 chars of stdout: {}",
                repr_string(&stdout.chars().take(500).collect::<String>())
            )
        })?;
    if let Value::Array(a) = value {
        Ok(a.iter().rev().find(|v|v["type"].as_str()==Some("result")).or_else(||a.last().filter(|v|v.is_object())).cloned().ok_or_else(||format!("claude -p returned a JSON array with no result object; first 500 chars of stdout: {}",repr_string(&stdout.chars().take(500).collect::<String>())))?)
    } else if value.is_object() {
        Ok(value)
    } else {
        Err("claude -p returned a non-object envelope".into())
    }
}
pub fn cli_error(stdout: &str) -> String {
    let Ok(e) = cli_envelope(stdout) else {
        return String::new();
    };
    if !truth(&e["is_error"]) {
        return String::new();
    }
    e["result"]
        .as_str()
        .map(crate::llm_config::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unspecified error")
        .to_owned()
}
pub fn mark_partial(result: &mut Value) {
    for key in KEYS {
        if let Some(a) = result[key].as_array_mut() {
            for v in a {
                if v.is_object() {
                    v["_partial"] = json!(true)
                }
            }
        }
    }
}
pub fn chunk_partial_files(chunk: &[Unit]) -> Vec<String> {
    chunk
        .iter()
        .map(|u| unit_path(u).to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub fn merged_partial_files(results: &[&Value]) -> Vec<String> {
    results
        .iter()
        .flat_map(|v| v["_partial_files"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub fn partial_source_files(v: &Value) -> Vec<String> {
    let mut out: BTreeSet<_> = merged_partial_files(&[v]).into_iter().collect();
    for key in KEYS {
        if let Some(a) = v[key].as_array() {
            for n in a {
                if truth(&n["_partial"]) && truth(&n["source_file"]) {
                    out.insert(string(&n["source_file"]));
                }
            }
        }
    }
    out.into_iter().collect()
}
pub fn strip_partial_markers(v: &mut Value) {
    for key in KEYS {
        if let Some(a) = v[key].as_array_mut() {
            for n in a {
                if let Some(m) = n.as_object_mut() {
                    m.shift_remove("_partial");
                }
            }
        }
    }
}
pub fn merge_into(merged: &mut Value, result: &Value) {
    for key in KEYS {
        if let Some(a) = result[key].as_array() {
            if merged[key].is_null() {
                merged[key] = json!([])
            }
            merged[key]
                .as_array_mut()
                .unwrap()
                .extend(a.iter().cloned())
        }
    }
    for key in ["input_tokens", "output_tokens"] {
        merged[key] = json!(merged[key].as_i64().unwrap_or(0) + result[key].as_i64().unwrap_or(0))
    }
    if truth(&result["_partial_files"]) {
        merged["_partial_files"] = json!(merged_partial_files(&[merged, result]))
    }
}
