// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Backend configuration and environment precedence from graphify/llm.py.
use crate::{Result, build::Diagnostics, dedup_strings::whitespace, python_compat::repr_string};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
};
pub type Environment = BTreeMap<String, String>;
pub fn get<'a>(env: &'a Environment, key: &str) -> &'a str {
    env.get(key).map(String::as_str).unwrap_or("")
}
pub fn trim(s: &str) -> &str {
    s.trim_matches(whitespace)
}
pub fn yes(env: &Environment, key: &str) -> bool {
    matches!(
        trim(get(env, key)).to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
pub fn parse_integer(s: &str) -> Option<i64> {
    let s = trim(s);
    let mut normalized = String::new();
    let mut digit = false;
    let mut underscore = false;
    for (i, c) in s.chars().enumerate() {
        if i == 0 && matches!(c, '+' | '-') {
            normalized.push(c);
            continue;
        }
        if c == '_' {
            if !digit || underscore {
                return None;
            }
            underscore = true;
            continue;
        }
        let d = crate::decimal_data::DECIMAL
            .iter()
            .find(|(a, b)| (*a..=*b).contains(&(c as u32)))
            .map(|(a, _)| (c as u32 - a) % 10)?;
        normalized.push(char::from_digit(d, 10)?);
        digit = true;
        underscore = false;
    }
    if !digit || underscore {
        None
    } else {
        normalized.parse().ok()
    }
}
/// The opt-in seed for built-in Ollama requests, `GRAPHICLE_OLLAMA_SEED`
/// (Graphify 0.9.86, #4168): `None` when unset or blank, an error unless it
/// is an ASCII integer in `0..=i64::MAX` (leading zeros allowed).
pub fn resolve_ollama_seed(env: &Environment) -> std::result::Result<Option<i64>, String> {
    const MAX: &str = "9223372036854775807";
    let value = trim(get(env, "GRAPHICLE_OLLAMA_SEED"));
    if value.is_empty() {
        return Ok(None);
    }
    let digits = value.trim_start_matches('0');
    let normalized = if digits.is_empty() { "0" } else { digits };
    if !value.bytes().all(|b| b.is_ascii_digit())
        || normalized.len() > MAX.len()
        || (normalized.len() == MAX.len() && normalized > MAX)
    {
        return Err(format!(
            "GRAPHICLE_OLLAMA_SEED must be an ASCII integer in the range 0..{MAX}."
        ));
    }
    normalized
        .parse::<i64>()
        .map(Some)
        .map_err(|e| e.to_string())
}
pub fn resolve_nonnegative(env: &Environment, key: &str, default: i64, positive: bool) -> i64 {
    parse_integer(get(env, key))
        .filter(|v| if positive { *v > 0 } else { *v >= 0 })
        .unwrap_or(default)
}
pub fn max_tokens(env: &Environment, default: i64) -> i64 {
    resolve_nonnegative(env, "GRAPHICLE_MAX_OUTPUT_TOKENS", default, true)
}
pub fn max_retries(env: &Environment, default: i64) -> i64 {
    resolve_nonnegative(env, "GRAPHICLE_MAX_RETRIES", default, false)
}
pub fn max_retry_depth(env: &Environment, default: i64) -> i64 {
    resolve_nonnegative(env, "GRAPHICLE_MAX_RETRY_DEPTH", default, false)
}
pub fn timeout(env: &Environment, default: f64) -> f64 {
    parse_float(get(env, "GRAPHICLE_API_TIMEOUT"))
        .filter(|v| *v > 0.0 && v.is_finite())
        .unwrap_or(default)
}
fn parse_float(s: &str) -> Option<f64> {
    let normalized: String = trim(s)
        .chars()
        .map(|c| {
            crate::decimal_data::DECIMAL
                .iter()
                .find(|(a, b)| (*a..=*b).contains(&(c as u32)))
                .and_then(|(a, _)| char::from_digit((c as u32 - a) % 10, 10))
                .unwrap_or(c)
        })
        .collect();
    let chars: Vec<_> = normalized.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_'
            && (i == 0
                || i + 1 == chars.len()
                || !chars[i - 1].is_ascii_digit()
                || !chars[i + 1].is_ascii_digit())
        {
            return None;
        }
    }
    normalized
        .replace('_', "")
        .to_ascii_lowercase()
        .parse()
        .ok()
}
pub fn requires_default_temperature(model: &str) -> bool {
    let low = model.to_lowercase();
    let base = low.rsplit('/').next().unwrap_or("");
    base.starts_with("gpt-5")
        || ["o1", "o3", "o4"]
            .iter()
            .any(|s| base == *s || base.starts_with(&format!("{s}-")))
}
pub fn temperature(
    env: &Environment,
    default: Option<f64>,
    model: &str,
    d: &mut Diagnostics,
) -> Option<f64> {
    let raw = trim(get(env, "GRAPHICLE_LLM_TEMPERATURE"));
    if !raw.is_empty() {
        if ["none", "omit", "default"].contains(&raw.to_lowercase().as_str()) {
            return None;
        }
        if let Some(v) = parse_float(raw) {
            return Some(v);
        }
        d.stderr.push(format!("[graphicle] GRAPHICLE_LLM_TEMPERATURE={} is not a number or 'none'; falling back to the backend default.",repr_string(raw)));
    }
    if requires_default_temperature(model) {
        None
    } else {
        default
    }
}
pub fn bedrock_inference(
    env: &Environment,
    max_tokens: i64,
    model: &str,
    d: &mut Diagnostics,
) -> Value {
    let mut v = json!({"maxTokens":max_tokens});
    if let Some(t) = temperature(env, Some(0.0), model, d) {
        v["temperature"] = json!(t)
    }
    v
}
pub fn resolve_ollama_base_url(env: &Environment, default: &str) -> String {
    if let Some(v) = env.get("OLLAMA_BASE_URL") {
        return v.clone();
    }
    let Some(raw) = env.get("OLLAMA_HOST") else {
        return default.into();
    };
    let raw = trim(raw);
    if raw.is_empty() {
        return default.into();
    }
    static DIGITS: std::sync::LazyLock<Vec<(u32, u32)>> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("assets/export/digit_ranges.json")).unwrap()
    });
    let digit = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| DIGITS.iter().any(|(a, b)| (*a..=*b).contains(&(c as u32))))
    };
    let mut host = if digit(raw) {
        format!("localhost:{raw}")
    } else if raw.starts_with(':') && digit(&raw[1..]) {
        format!("localhost{raw}")
    } else {
        raw.into()
    };
    if !host.starts_with("http://") && !host.starts_with("https://") {
        host = format!("http://{host}")
    }
    // Keep explicit ports (including :80/:443) and URL spelling; URL serializers
    // would erase default ports that the upstream urllib parser retains.
    if let Some((scheme, rest)) = host.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        let after_user = authority.rsplit('@').next().unwrap_or("");
        let parsed = if after_user.starts_with('[') {
            after_user.find(']').map(|i| {
                (
                    &after_user[..=i],
                    after_user[i + 1..].strip_prefix(':').unwrap_or(""),
                )
            })
        } else if let Some((name, port)) = after_user.rsplit_once(':') {
            (!name.contains(':')).then_some((name, port))
        } else {
            Some((after_user, ""))
        };
        if let Some((name, port)) = parsed.filter(|(name, _)| !name.is_empty())
            && port.is_empty()
        {
            let userinfo = authority
                .rsplit_once('@')
                .map(|(user, _)| format!("{user}@"))
                .unwrap_or_default();
            host = format!(
                "{scheme}://{userinfo}{}:11434{}",
                name.to_lowercase(),
                &rest[end..]
            );
        }
    }
    host = host.trim_end_matches('/').to_owned();
    if !host.ends_with("/v1") {
        host.push_str("/v1")
    }
    host
}
fn url_parts(url: &str) -> Result<(String, String)> {
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s.to_lowercase())
        .unwrap_or_default();
    if !["http", "https"].contains(&scheme.as_str()) {
        return Ok((scheme, String::new()));
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "unparseable URL")?;
    Ok((
        scheme,
        parsed
            .host_str()
            .unwrap_or("")
            .trim_matches(['[', ']'])
            .to_lowercase(),
    ))
}
fn loopback(host: &str) -> bool {
    ["localhost", "127.0.0.1", "::1"].contains(&host) || host.starts_with("127.")
}
pub fn provider_base_url_ok(url: &str, name: &str, warn: bool, d: &mut Diagnostics) -> bool {
    let Ok((scheme, host)) = url_parts(url) else {
        if warn {
            d.stderr.push(format!(
                "[graphicle] WARNING: provider {} has an unparseable base_url; ignoring.",
                repr_string(name)
            ))
        }
        return false;
    };
    if !["http", "https"].contains(&scheme.as_str()) {
        if warn {
            d.stderr.push(format!(
                "[graphicle] WARNING: provider {} base_url scheme {} is not http/https; ignoring.",
                repr_string(name),
                repr_string(&scheme)
            ))
        }
        return false;
    }
    if warn && scheme == "http" && !loopback(&host) {
        d.stderr.push(format!("[graphicle] WARNING: provider {} sends your corpus to {} over plaintext http. Use https unless this is a trusted local endpoint.",repr_string(name),repr_string(&host)))
    }
    true
}
pub fn metadata_host_with(host: &str, addresses: &[IpAddr]) -> bool {
    [
        "metadata.google.internal",
        "metadata.google.com",
        "0.0.0.0",
        "::",
        "[::]",
    ]
    .contains(&host)
        || host.starts_with("169.254.")
        || addresses.iter().any(|ip| match ip {
            IpAddr::V4(a) => a.is_link_local(),
            IpAddr::V6(a) => a.is_unicast_link_local(),
        })
}
pub fn metadata_host(host: &str) -> bool {
    if metadata_host_with(host, &[]) {
        return true;
    }
    let ips = (host, 0)
        .to_socket_addrs()
        .map(|v| v.map(|a| a.ip()).collect::<Vec<_>>())
        .unwrap_or_default();
    metadata_host_with(host, &ips)
}
pub fn validate_ollama_url_with(
    url: &str,
    warn: bool,
    blocked: impl FnOnce(&str) -> bool,
    d: &mut Diagnostics,
) -> Result<()> {
    let Ok((scheme, host)) = url_parts(url) else {
        if warn {
            d.stderr.push(format!(
                "[graphicle] WARNING: OLLAMA_BASE_URL={} is not a parseable URL.",
                repr_string(url)
            ))
        }
        return Ok(());
    };
    if !["http", "https"].contains(&scheme.as_str()) {
        if warn {
            d.stderr.push(format!("[graphicle] WARNING: OLLAMA_BASE_URL has unexpected scheme {}; expected http or https.",repr_string(&scheme)))
        }
        return Ok(());
    }
    if blocked(&host) {
        return Err(format!(
            "OLLAMA_BASE_URL points at a link-local/metadata address ({}); refusing to send the corpus there. Set it to a real Ollama host.",
            repr_string(&host)
        ).into());
    }
    if warn && !loopback(&host) {
        d.stderr.push(format!("[graphicle] WARNING: OLLAMA_BASE_URL points to non-loopback host {}{}. Your full corpus will be sent to that endpoint. Set OLLAMA_BASE_URL=http://localhost:11434/v1 to keep extraction local.",repr_string(&host),if scheme=="http"{" (UNENCRYPTED)"}else{""}))
    }
    Ok(())
}

#[derive(Clone)]
pub struct Configuration {
    pub backends: Map<String, Value>,
    pub env: Environment,
}
impl Configuration {
    pub fn builtins(env: Environment) -> Self {
        let mut backends = serde_json::from_str::<Value>(include_str!("assets/llm/backends.json"))
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        for (backend, key, env_key) in [
            ("claude", "base_url", "ANTHROPIC_BASE_URL"),
            ("claude", "default_model", "ANTHROPIC_MODEL"),
            ("kimi", "base_url", "KIMI_BASE_URL"),
            ("ollama", "default_model", "OLLAMA_MODEL"),
            ("gemini", "base_url", "GEMINI_BASE_URL"),
            ("openai", "base_url", "OPENAI_BASE_URL"),
            ("openai", "default_model", "OPENAI_MODEL"),
            ("deepseek", "base_url", "DEEPSEEK_BASE_URL"),
        ] {
            if let Some(v) = env.get(env_key) {
                backends[backend][key] = json!(v)
            }
        }
        backends["ollama"]["base_url"] =
            json!(resolve_ollama_base_url(&env, "http://localhost:11434/v1"));
        if let Some(v) = env
            .get("AZURE_OPENAI_DEPLOYMENT")
            .or_else(|| env.get("GRAPHICLE_AZURE_MODEL"))
        {
            backends["azure"]["default_model"] = json!(v)
        }
        Self { backends, env }
    }
    pub fn from_environment(d: &mut Diagnostics) -> Self {
        let mut cfg = Self::builtins(std::env::vars().collect());
        let global = std::env::home_dir()
            .unwrap_or_default()
            .join(".graphicle-graph/providers.json");
        cfg.load_custom(&global, Path::new(".graphicle-graph/providers.json"), d);
        cfg
    }
    pub fn load_custom(&mut self, global: &Path, local: &Path, d: &mut Diagnostics) {
        let allow = ["1", "true", "yes"].contains(
            &trim(get(&self.env, "GRAPHICLE_ALLOW_LOCAL_PROVIDERS"))
                .to_lowercase()
                .as_str(),
        );
        if local.is_file() && !allow {
            d.stderr.push(format!("[graphicle] WARNING: ignoring project-local {} (custom providers control where your corpus and API key are sent). Set GRAPHICLE_ALLOW_LOCAL_PROVIDERS=1 to load it.",local.display()))
        }
        let sources = if allow {
            vec![local, global]
        } else {
            vec![global]
        };
        for path in sources {
            let Some(data) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            else {
                continue;
            };
            let Some(rows) = data.as_object() else {
                continue;
            };
            for (name, cfg) in rows {
                let Some(cfg) = cfg.as_object() else { continue };
                if self.backends.contains_key(name) {
                    continue;
                }
                let url = crate::python_compat::string(cfg.get("base_url").unwrap_or(&json!("")));
                if !provider_base_url_ok(&url, name, true, d) {
                    continue;
                }
                let mut cfg = cfg.clone();
                cfg.entry("pricing")
                    .or_insert(json!({"input":0.0,"output":0.0}));
                self.backends.insert(name.clone(), json!(cfg));
            }
        }
    }
    pub fn backend(&self, name: &str) -> Result<&Value> {
        Ok(self
            .backends
            .get(name)
            .ok_or_else(|| format!("Unknown backend {}", repr_string(name)))?)
    }
    pub fn env_keys(&self, backend: &str) -> Result<Vec<String>> {
        let cfg = self.backend(backend)?;
        if let Some(v) = cfg
            .get("env_keys")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            return v
                .iter()
                .map(|s| {
                    s.as_str()
                        .map(str::to_owned)
                        .ok_or("Expected API-key environment name".into())
                })
                .collect();
        }
        Ok(cfg["env_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.into()])
            .unwrap_or_default())
    }
    pub fn api_key(&self, backend: &str) -> Result<String> {
        Ok(self
            .env_keys(backend)?
            .iter()
            .find_map(|k| self.env.get(k).filter(|v| !v.is_empty()).cloned())
            .unwrap_or_default())
    }
    pub fn format_env_keys(&self, backend: &str) -> Result<String> {
        let keys = self.env_keys(backend)?;
        Ok(if keys.is_empty() {
            "AWS_PROFILE or AWS_REGION".into()
        } else {
            keys.join(" or ")
        })
    }
    pub fn model(&self, backend: &str) -> Result<String> {
        let cfg = self.backend(backend)?;
        if let Some(key) = cfg["model_env_key"].as_str() {
            let v = get(&self.env, key);
            if !v.is_empty() {
                return Ok(v.into());
            }
        }
        cfg["default_model"]
            .as_str()
            .map(str::to_owned)
            .ok_or("Backend has no default model".into())
    }
    pub fn vision(&self, backend: &str) -> bool {
        if backend == "ollama" {
            trim(get(&self.env, "GRAPHICLE_OLLAMA_VISION")) == "1"
        } else {
            self.backends
                .get(backend)
                .is_some_and(|v| crate::exporters::common::truth(&v["vision"]))
        }
    }
    pub fn detection_env_vars(&self) -> Vec<String> {
        let mut out = vec![];
        for name in self.backends.keys() {
            for key in self.env_keys(name).unwrap_or_default() {
                if !out.contains(&key) {
                    out.push(key)
                }
            }
        }
        for key in [
            "AZURE_OPENAI_ENDPOINT",
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "OLLAMA_BASE_URL",
            "OLLAMA_HOST",
        ] {
            if !out.iter().any(|v| v == key) {
                out.push(key.into())
            }
        }
        out
    }
    pub fn detect_backend(&self, d: &mut Diagnostics) -> Result<Option<String>> {
        for backend in ["gemini", "kimi", "claude", "openai", "deepseek"] {
            if !self.api_key(backend)?.is_empty() {
                return Ok(Some(backend.into()));
            }
        }
        if !self.api_key("azure")?.is_empty() && !get(&self.env, "AZURE_OPENAI_ENDPOINT").is_empty()
        {
            return Ok(Some("azure".into()));
        }
        if ["AWS_PROFILE", "AWS_REGION", "AWS_DEFAULT_REGION"]
            .iter()
            .any(|k| !get(&self.env, k).is_empty())
        {
            return Ok(Some("bedrock".into()));
        }
        let url = resolve_ollama_base_url(&self.env, "");
        if !url.is_empty() {
            validate_ollama_url_with(&url, true, metadata_host, d)?;
            return Ok(Some("ollama".into()));
        }
        for backend in self.backends.keys() {
            if ![
                "gemini",
                "kimi",
                "claude",
                "openai",
                "deepseek",
                "azure",
                "bedrock",
                "ollama",
                "claude-cli",
            ]
            .contains(&backend.as_str())
                && !self.api_key(backend)?.is_empty()
            {
                return Ok(Some(backend.clone()));
            }
        }
        Ok(None)
    }
    pub fn estimate_cost(&self, backend: &str, input: f64, output: f64) -> f64 {
        self.backends
            .get(backend)
            .map(|c| {
                (input * c["pricing"]["input"].as_f64().unwrap_or(0.0)
                    + output * c["pricing"]["output"].as_f64().unwrap_or(0.0))
                    / 1e6
            })
            .unwrap_or(0.0)
    }
}
/// A variable from `env`. Windows variable names are case-insensitive, so a
/// missing exact name falls back to any spelling (`Path` for `PATH`).
fn env_value<'a>(env: &'a Environment, key: &str) -> &'a str {
    #[cfg(windows)]
    if !env.contains_key(key)
        && let Some(value) = env
            .iter()
            .find_map(|(k, v)| k.eq_ignore_ascii_case(key).then_some(v))
    {
        return value;
    }
    get(env, key)
}
pub fn executable(name: &str, env: &Environment) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = env_value(env, "PATH");
    for directory in std::env::split_paths(path) {
        let p = directory.join(name);
        if p.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if p.metadata()
                    .ok()
                    .is_none_or(|m| m.permissions().mode() & 0o111 == 0)
                {
                    continue;
                }
            }
            return Some(p);
        }
        #[cfg(windows)]
        for ext in env_value(env, "PATHEXT").split(';') {
            let candidate = directory.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
pub fn claude_command(env: &Environment) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(p) = executable("claude.cmd", env) {
        return Some(p);
    }
    executable("claude", env)
}

#[cfg(all(test, windows))]
mod windows_path_tests {
    use super::{Environment, executable};

    #[test]
    fn path_and_pathext_are_found_in_any_case() {
        let system = std::path::Path::new(&std::env::var("SystemRoot").unwrap()).join("System32");
        let env: Environment = [
            ("Path".to_string(), system.to_string_lossy().into_owned()),
            ("PathExt".to_string(), ".COM;.EXE".to_string()),
        ]
        .into_iter()
        .collect();
        let found = executable("cmd", &env).unwrap();
        assert!(found.starts_with(&system), "{}", found.display());
    }
}
