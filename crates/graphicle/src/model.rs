//! Bounded inference transports. Workspace vaults resolve credentials before dispatch.
use crate::{Error, Result, digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    #[serde(default, alias = "credential_ref")]
    pub key_env: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default = "input_default")]
    pub input_limit: usize,
    #[serde(default = "output_default")]
    pub output_limit: usize,
    #[serde(default = "enabled")]
    pub structured_output: bool,
    /// Vendor request fields for chat-completions servers (vLLM, SGLang,
    /// llama.cpp), e.g. `chat_template_kwargs` or `top_k`. Fields the runtime
    /// owns (messages, budgets, output format, sampling temperature) are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,
}
/// Chat request fields that carry the runtime's prompt, budget and output contract.
const OWNED_CHAT_FIELDS: [&str; 13] = [
    "model",
    "messages",
    "max_completion_tokens",
    "max_tokens",
    "response_format",
    "temperature",
    "stream",
    "stream_options",
    "n",
    "tools",
    "tool_choice",
    "functions",
    "function_call",
];
fn enabled() -> bool {
    true
}
fn input_default() -> usize {
    16000
}
fn output_default() -> usize {
    2048
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, Profile>,
    #[serde(default = "concurrency_default")]
    pub worker_concurrency: usize,
    #[serde(default)]
    pub connector_endpoints: BTreeMap<String, String>,
    /// G08 ingestion bounds; `None` selects `ingestion::Bounds` defaults.
    #[serde(default)]
    pub max_cpu_jobs: Option<usize>,
    #[serde(default)]
    pub max_model_jobs: Option<usize>,
    #[serde(default)]
    pub max_queued_bytes_per_repository: Option<usize>,
    /// Freshness before answers (`crate::fresh`): `auto` (the default)
    /// syncs a folder project's few changed files before `ask`, `context`,
    /// `status` and MCP tool calls; `off` never checks. The environment
    /// variable `GRAPHICLE_FRESHNESS` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::fresh::Mode>,
    /// Core size limits (`limits.project_files`, ...; [`crate::limits`]).
    /// `GRAPHICLE_MAX_*` environment variables win over these.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, Value>,
    /// The new-release notice after interactive commands
    /// ([`crate::update_notice`]): `false` turns it off, as the environment
    /// variable `GRAPHICLE_NO_UPDATE_NOTIFIER` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_notifier: Option<bool>,
}
fn concurrency_default() -> usize {
    1
}
impl Default for Config {
    fn default() -> Self {
        Self {
            default_model: None,
            embedding_model: None,
            models: BTreeMap::new(),
            worker_concurrency: 1,
            connector_endpoints: BTreeMap::new(),
            max_cpu_jobs: None,
            max_model_jobs: None,
            max_queued_bytes_per_repository: None,
            freshness: None,
            limits: BTreeMap::new(),
            update_notifier: None,
        }
    }
}
impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = if let Some(path) = path {
            let raw = std::fs::read(path)?;
            if raw.len() > 65536 {
                return Err(Error::bad("Provider configuration exceeds 64 KiB"));
            }
            serde_json::from_slice(&raw)?
        } else {
            Self {
                worker_concurrency: 1,
                ..Self::default()
            }
        };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        for (kind, endpoint) in &self.connector_endpoints {
            if ![
                "github",
                "slack",
                "notion",
                "gdrive",
                "gdrive_directory",
                "jira",
                "confluence",
            ]
            .contains(&kind.as_str())
            {
                return Err(Error::bad("Unknown connector endpoint"));
            }
            checked_url(endpoint)?;
        }
        if self.models.len() > 20 || !(1..=64).contains(&self.worker_concurrency) {
            return Err(Error::bad(
                "Invalid provider or worker limit (at most 20 models; worker_concurrency 1 to 64)",
            ));
        }
        if self.max_cpu_jobs.is_some_and(|n| !(1..=64).contains(&n))
            || self.max_model_jobs.is_some_and(|n| !(1..=64).contains(&n))
            || self
                .max_queued_bytes_per_repository
                .is_some_and(|n| n < crate::ingestion::MIN_QUEUED_BYTES)
        {
            return Err(Error::bad("Invalid ingestion bounds"));
        }
        crate::limits::Limits::resolve(&self.limits, |_| None)?;
        for (name, p) in &self.models {
            if name.is_empty()
                || name.len() > 100
                || p.model.is_empty()
                || p.model.len() > 200
                || ![
                    "responses",
                    "chat",
                    "ollama",
                    "anthropic",
                    "gemini",
                    "azure_responses",
                    "azure_chat",
                    "local_onnx",
                ]
                .contains(&p.protocol.as_str())
                || !(512..=128000).contains(&p.input_limit)
                || !(64..=16000).contains(&p.output_limit)
                || p.temperature
                    .is_some_and(|t| !t.is_finite() || !(0.0..=2.0).contains(&t))
            {
                return Err(Error::bad("Invalid model profile"));
            }
            if let Some(extra) = &p.extra_body
                && (!matches!(p.protocol.as_str(), "chat" | "azure_chat")
                    || extra.len() > 16
                    || serde_json::to_vec(extra).map_or(true, |bytes| bytes.len() > 4096)
                    || extra
                        .keys()
                        .any(|k| k.is_empty() || OWNED_CHAT_FIELDS.contains(&k.as_str())))
            {
                return Err(Error::bad(
                    "Invalid extra_body: chat profiles only, at most 16 fields and 4 KiB, no runtime-owned fields",
                ));
            }
            if p.protocol == "local_onnx" {
                crate::local_inference::validate(p)?;
            } else {
                provider_url(&p.base_url, p.key_env.is_some())?;
            }
            if p.key_env.as_ref().is_some_and(|key| {
                key.is_empty()
                    || key.len() > 100
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'-')
            }) {
                return Err(Error::bad(
                    "Use a stored credential reference or an environment variable name",
                ));
            }
        }
        for name in [&self.default_model, &self.embedding_model]
            .into_iter()
            .flatten()
        {
            if !self.models.contains_key(name) {
                return Err(Error::bad("Default model is not configured"));
            }
        }
        Ok(())
    }
    pub fn status(&self) -> Value {
        json!({"default_model":self.default_model,"embedding_model":self.embedding_model,
            "models":self.models.iter().map(|(name,p)|json!({"name":name,"protocol":p.protocol,"model":p.model,
                "credentials_available":p.key_env.as_ref().is_none_or(|v|std::env::var(v).is_ok()),"input_limit":p.input_limit,"output_limit":p.output_limit,"structured_output":p.structured_output})).collect::<Vec<_>>()})
    }
    pub fn profile(&self, name: Option<&str>) -> Result<(&str, &Profile)> {
        let name = name.or(self.default_model.as_deref()).ok_or_else(|| {
            Error::bad("Configure a local or API model before starting inference")
        })?;
        self.models
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| Error::bad("Unknown configured model"))
    }
}
/// An outbound URL the service may call: HTTPS, or HTTP on loopback, with no
/// credentials, query or fragment. Connectors, OAuth and every other caller
/// use this rule; model profiles use [`provider_url`].
pub fn checked_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value).map_err(|_| Error::bad("Invalid provider URL"))?;
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback(&url))) || !bare(&url) {
        return Err(Error::bad(
            "Use HTTPS, or HTTP on loopback, without credentials or a query in the URL",
        ));
    }
    Ok(url)
}
fn loopback(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}
/// No user name, password, query or fragment.
fn bare(url: &url::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

/// Opt-in plain-HTTP model hosts (EVAL_PLAN R5, P-URL): a model server on a
/// private network, such as a vLLM host, that serves no TLS. The variable
/// lists exact host names or IP addresses, separated by commas or spaces,
/// without scheme, port or path (IPv6 in brackets, as in a URL). Only model
/// profiles read it.
pub const PROVIDER_HTTP_HOSTS: &str = "GRAPHICLE_PROVIDER_HTTP_HOSTS";

/// The hosts [`PROVIDER_HTTP_HOSTS`] lists; an entry that is not exactly a
/// host is refused rather than ignored.
pub fn provider_http_hosts() -> Result<Vec<String>> {
    http_hosts(&std::env::var(PROVIDER_HTTP_HOSTS).unwrap_or_default())
}
fn http_hosts(value: &str) -> Result<Vec<String>> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            // Exact means the URL parser leaves the entry as written: no
            // port, path, user, wildcard or non-canonical address.
            url::Url::parse(&format!("http://{entry}/"))
                .ok()
                .filter(|url| {
                    url.host_str() == Some(entry)
                        && !entry.contains('*')
                        && url.port().is_none()
                        && url.path() == "/"
                        && bare(url)
                })
                .map(|_| entry.to_string())
                .ok_or_else(|| {
                    Error::bad(format!(
                        "{PROVIDER_HTTP_HOSTS} lists exact hosts (lowercase names or IP addresses, without scheme, port or path); {entry:?} is not one"
                    ))
                })
        })
        .collect()
}

/// A model profile's base URL: [`checked_url`]'s rule, or plain HTTP to a host
/// [`PROVIDER_HTTP_HOSTS`] lists, matched exactly. A profile that sends a
/// credential (`key_env`) never uses plain HTTP off loopback.
pub fn provider_url(value: &str, credential: bool) -> Result<url::Url> {
    provider_url_with(value, credential, &provider_http_hosts()?)
}
pub fn provider_url_with(value: &str, credential: bool, hosts: &[String]) -> Result<url::Url> {
    let url = url::Url::parse(value).map_err(|_| Error::bad("Invalid provider URL"))?;
    let listed = url.scheme() == "http"
        && !loopback(&url)
        && url
            .host_str()
            .is_some_and(|host| hosts.iter().any(|h| h == host));
    if !listed {
        return checked_url(value).map_err(|_| {
            Error::bad(format!(
                "Use HTTPS, HTTP on loopback, or HTTP to a host listed in {PROVIDER_HTTP_HOSTS}, without credentials or a query in the URL"
            ))
        });
    }
    if !bare(&url) {
        return Err(Error::bad(
            "A provider URL carries no credentials, query or fragment",
        ));
    }
    if credential {
        return Err(Error::bad(format!(
            "A provider credential (key_env) is never sent over plain HTTP: use HTTPS for this host, or remove key_env (hosts in {PROVIDER_HTTP_HOSTS} serve without one)"
        )));
    }
    Ok(url)
}
pub fn client() -> Result<reqwest::Client> {
    client_with_timeout(Duration::from_secs(180))
}
pub fn client_with_timeout(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()
        .map_err(|_| Error::bad("Cannot construct HTTP client"))
}
pub async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(Error::bad(format!(
            "Provider returned HTTP {}",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > 4 * 1024 * 1024)
    {
        return Err(Error::bad("Provider response exceeds 4 MiB"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::bad("Provider response interrupted"))?
    {
        if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
            return Err(Error::bad("Provider response exceeds 4 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::bad("Provider did not return JSON"))
}
/// A model provider's reply as JSON: [`bounded_json`], except that a refused
/// request keeps the provider's own reason (see [`refusal_message`]), so the
/// person asking learns what the provider objected to: an unknown model, an
/// input over its context window, a schema it does not take, a bad key.
async fn provider_json(mut response: reqwest::Response, credential: Option<&str>) -> Result<Value> {
    let status = response.status();
    if status.is_success() {
        return bounded_json(response).await;
    }
    let mut body = Vec::new();
    while body.len() < REFUSAL_BODY_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    Err(Error::bad(refusal_message(
        status.as_u16(),
        &body,
        credential,
    )))
}
/// Bytes of a refused request's body that are read for its reason.
const REFUSAL_BODY_BYTES: usize = 65536;
/// Characters of a provider's reason that an error repeats.
const REASON_CHARS: usize = 300;
/// `Provider returned HTTP N`, then `: ` and the provider's reason when its
/// body gives one: the `error.message` of OpenAI-style, Anthropic, Gemini and
/// vLLM replies, Ollama's and llama.cpp's `error` text, or a `detail` or
/// `message` field; else short plain text (never an HTML page). The reason
/// is one line of at most 300 characters, and the credential, or anything
/// shaped like an API key, never appears in it.
pub fn refusal_message(status: u16, body: &[u8], credential: Option<&str>) -> String {
    match provider_reason(body, credential) {
        Some(reason) => format!("Provider returned HTTP {status}: {reason}"),
        None => format!("Provider returned HTTP {status}"),
    }
}
fn provider_reason(body: &[u8], credential: Option<&str>) -> Option<String> {
    let text = String::from_utf8_lossy(&body[..body.len().min(REFUSAL_BODY_BYTES)]);
    let reason = match serde_json::from_str::<Value>(&text) {
        Ok(value) => {
            let text = |field: &Value| {
                field
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            };
            let detail = &value["detail"];
            text(&value["error"]["message"])
                .or_else(|| text(&value["error"]))
                .or_else(|| text(detail))
                .or_else(|| text(&value["message"]))
                .or_else(|| (detail.is_array() || detail.is_object()).then(|| detail.to_string()))?
        }
        Err(_) if !text.trim_start().starts_with('<') => text.into_owned(),
        Err(_) => return None,
    };
    let mut words = Vec::new();
    for word in reason.split_whitespace() {
        let keylike = ["sk-", "sk_", "AIza", "xai-", "gsk_"].iter().any(|prefix| {
            word.trim_start_matches(['"', '\'', '('])
                .starts_with(prefix)
        });
        words.push(if keylike {
            "[redacted]".to_string()
        } else {
            word.to_string()
        });
    }
    let mut line = words.join(" ");
    if let Some(secret) = credential.filter(|secret| secret.len() >= 4) {
        line = line.replace(secret, "[redacted]");
    }
    line.retain(|c| !c.is_control());
    if line.is_empty() {
        return None;
    }
    Some(match line.char_indices().nth(REASON_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line,
    })
}
pub fn key(p: &Profile) -> String {
    if p.protocol == "local_onnx" {
        // `runs`: every vector comes from a model run of its text alone
        // (local_inference::each_alone). Vectors stored or cached under the
        // earlier key came from batched runs, so they no longer match: an
        // index built before is reported unavailable until `embed` runs again.
        return digest(
            &json!({"protocol":p.protocol,"base_url":p.base_url,"model":p.model,
            "manifest_sha256":crate::local_inference::fingerprint(p).ok(),"input_limit":p.input_limit,
            "runs":"one_text"}),
        );
    }
    let mut identity = json!({"protocol":p.protocol,"base_url":p.base_url,"model":p.model,"temperature":p.temperature,"input_limit":p.input_limit,"output_limit":p.output_limit,"structured_output":p.structured_output});
    // Only when set, so profiles without request fields keep their identity.
    if let Some(extra) = &p.extra_body {
        identity["extra_body"] = json!(extra);
    }
    digest(&identity)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub text: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub usage_reported: bool,
    pub elapsed_ms: u64,
    pub model: String,
}
impl Reply {
    pub fn json(&self) -> Result<Value> {
        let trimmed = self.text.trim();
        // Reasoning models served without a reasoning parser lead with their thinking.
        let trimmed = trimmed
            .strip_prefix("<think>")
            .and_then(|rest| rest.split_once("</think>"))
            .map_or(trimmed, |(_, answer)| answer.trim());
        let json_text = trimmed
            .strip_prefix("```json\n")
            .or_else(|| trimmed.strip_prefix("```\n"))
            .and_then(|s| s.strip_suffix("```"))
            .unwrap_or(trimmed);
        serde_json::from_str(json_text.trim()).map_err(|_| {
            Error::bad("Model returned invalid structured JSON; no memory was committed")
        })
    }
    /// Some non-constrained local models return the sole array without its object
    /// envelope, or (over OpenAI-compatible transports; Hindsight #4667) send an
    /// array or object property as its JSON text. This restores only those
    /// unambiguous encodings, never missing facts, enum values, citations or
    /// dates. The complete schema is still validated.
    pub fn json_for(&self, schema: &Value) -> Result<Value> {
        let mut value = self.json()?;
        if value.is_array()
            && schema["type"] == "object"
            && let Some(properties) = schema["properties"].as_object().filter(|p| p.len() == 1)
        {
            let (name, definition) = properties.iter().next().unwrap();
            if definition["type"] == "array" {
                return Ok(json!({name:value}));
            }
        }
        if let (Some(object), Some(properties)) =
            (value.as_object_mut(), schema["properties"].as_object())
        {
            for (name, definition) in properties {
                let expected = definition["type"].as_str();
                if !matches!(expected, Some("array" | "object")) {
                    continue;
                }
                let Some(Value::String(text)) = object.get(name) else {
                    continue;
                };
                // Only text that decodes to the required type; anything else
                // stays as sent and fails validation.
                if let Ok(decoded) = serde_json::from_str::<Value>(text)
                    && (decoded.is_array() && expected == Some("array")
                        || decoded.is_object() && expected == Some("object"))
                {
                    object.insert(name.clone(), decoded);
                }
            }
        }
        Ok(value)
    }
}
fn authenticated(
    client: &reqwest::Client,
    p: &Profile,
    suffix: &str,
    credential: Option<&str>,
) -> Result<reqwest::RequestBuilder> {
    let endpoint = format!(
        "{}/{}",
        p.base_url.trim_end_matches('/'),
        suffix.trim_start_matches('/')
    );
    let mut request = client.post(endpoint);
    if p.key_env.is_some() && credential.is_none() {
        return Err(Error::bad("Configured provider credential is unavailable"));
    }
    if let Some(secret) = credential {
        request = match p.protocol.as_str() {
            "anthropic" => request.header("x-api-key", secret),
            "gemini" => request.header("x-goog-api-key", secret),
            "azure_responses" | "azure_chat" => request.header("api-key", secret),
            _ => request.bearer_auth(secret),
        };
    }
    if p.protocol == "anthropic" {
        request = request.header("anthropic-version", "2023-06-01");
    }
    Ok(request)
}
pub fn request_tokens(instructions: &str, input: &Value, schema: &Value) -> usize {
    // A reference estimate, not the configured provider's tokenizer.
    graphicle_memory::text::count(instructions)
        + graphicle_memory::text::count(&input.to_string())
        + graphicle_memory::text::count(&schema.to_string())
        + 64
}
pub fn strict_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    if let Some(object) = out.as_object_mut() {
        object.remove("default");
        if let Some(value) = object.remove("oneOf") {
            object.insert("anyOf".into(), value);
        }
        if let Some(value) = object.remove("const") {
            object.insert("enum".into(), json!([value]));
        }
        if !object.contains_key("type")
            && object
                .get("enum")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().all(Value::is_string))
        {
            object.insert("type".into(), json!("string"));
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            object.insert(
                "required".into(),
                json!(properties.keys().collect::<Vec<_>>()),
            );
            object.insert("additionalProperties".into(), json!(false));
        }
        for (key, value) in object.iter_mut() {
            if key == "properties" {
                if let Some(properties) = value.as_object_mut() {
                    for child in properties.values_mut() {
                        *child = strict_schema(child);
                    }
                }
            } else if ["items", "anyOf", "oneOf", "allOf"].contains(&key.as_str()) {
                *value = strict_schema(value);
            }
        }
    } else if let Some(items) = out.as_array_mut() {
        for value in items {
            *value = strict_schema(value);
        }
    }
    out
}
fn environment_credential(p: &Profile) -> Result<Option<String>> {
    p.key_env.as_deref().map(|name|std::env::var(name).ok().filter(|v|!v.is_empty()).ok_or_else(||Error::bad("Provider credential unavailable; workspace credentials require the service worker"))).transpose()
}
pub async fn generate(
    client: &reqwest::Client,
    p: &Profile,
    instructions: &str,
    input: &Value,
    schema: &Value,
    output_limit: usize,
) -> Result<Reply> {
    generate_with_credential(
        client,
        p,
        instructions,
        input,
        schema,
        output_limit,
        environment_credential(p)?.as_deref(),
    )
    .await
}
/// Provider-reported failure metadata. Reserved token limits are never substituted
/// for actual usage, and no partial model output is accepted as a valid result.
#[derive(Debug)]
pub struct Failure {
    pub reason: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub elapsed_ms: u64,
    pub model: String,
}

pub async fn generate_with_credential(
    client: &reqwest::Client,
    p: &Profile,
    instructions: &str,
    input: &Value,
    schema: &Value,
    output_limit: usize,
    credential: Option<&str>,
) -> Result<Reply> {
    let estimated = request_tokens(instructions, input, schema);
    if estimated > p.input_limit || output_limit < 64 || output_limit > p.output_limit {
        return Err(Error::bad(
            "Inference request exceeds the configured model budget",
        ));
    }
    let (suffix, body) = request_body(p, instructions, input, schema, output_limit)?;
    let start = Instant::now();
    // No hidden HTTP retries: the durable job's attempt and call budget own them.
    let response = authenticated(client, p, &suffix, credential)?
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            Error::bad(if error.is_timeout() && error.is_connect() {
                "Provider connection timed out before a response"
            } else if error.is_timeout() {
                "Provider request timed out; usage may be unknown"
            } else if error.is_connect() {
                "Could not establish the provider connection"
            } else {
                "Provider request failed before a response; usage may be unknown"
            })
        })?;
    let value = provider_json(response, credential).await?;
    reply(p, &value, estimated, start)
}

/// What the Anthropic transport's structured mode adds to the instructions:
/// the tool is offered with `tool_choice: auto`, because current Claude
/// models (Claude Opus 5.5, Claude Sonnet 5.5, Claude Fable 5.1) refuse a
/// forced tool choice with HTTP 400.
const ANTHROPIC_RESULT_TOOL: &str =
    "Return the result by calling the graphicle_result tool exactly once.";

/// The schema as a provider takes it: [`strict_schema`], and for the
/// OpenAI-style protocols without the string-length keywords (`minLength`,
/// `maxLength`), which OpenAI's strict structured outputs refuse. The reply
/// is still validated against the full schema.
pub fn provider_schema(protocol: &str, schema: &Value) -> Value {
    let mut out = strict_schema(schema);
    if matches!(
        protocol,
        "responses" | "azure_responses" | "chat" | "azure_chat"
    ) {
        drop_string_lengths(&mut out);
    }
    out
}
fn drop_string_lengths(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            object.remove("minLength");
            object.remove("maxLength");
            for (key, value) in object.iter_mut() {
                if key == "properties" {
                    if let Some(properties) = value.as_object_mut() {
                        properties.values_mut().for_each(drop_string_lengths);
                    }
                } else if ["items", "anyOf", "oneOf", "allOf"].contains(&key.as_str()) {
                    drop_string_lengths(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(drop_string_lengths),
        _ => {}
    }
}

/// The endpoint (after the profile's base URL) and the request body of one
/// generation, in the profile's protocol: the instructions, the input as
/// JSON text, the output contract ([`provider_schema`], or the schema in the
/// instructions when `structured_output` is off), the output budget and the
/// sampling settings.
pub fn request_body(
    p: &Profile,
    instructions: &str,
    input: &Value,
    schema: &Value,
    output_limit: usize,
) -> Result<(String, Value)> {
    let user = input.to_string();
    // The prompt of a profile without structured output shows the whole schema.
    let full = strict_schema(schema);
    let schema = provider_schema(&p.protocol, schema);
    let gemini_suffix = format!(
        "models/{}:generateContent",
        url::form_urlencoded::byte_serialize(p.model.trim_start_matches("models/").as_bytes())
            .collect::<String>()
    );
    let (suffix, mut body) = match p.protocol.as_str() {
        "responses" | "azure_responses" => (
            "responses".to_string(),
            json!({"model":p.model,"instructions":instructions,"input":user,"store":false,
            "max_output_tokens":output_limit,"text":{"format":{"type":"json_schema","name":"graphicle_result","strict":true,"schema":schema}}}),
        ),
        "chat" | "azure_chat" => (
            "chat/completions".to_string(),
            json!({"model":p.model,"messages":[{"role":"system","content":instructions},{"role":"user","content":user}],
            "max_completion_tokens":output_limit,"response_format":{"type":"json_schema","json_schema":{"name":"graphicle_result","strict":true,"schema":schema}}}),
        ),
        // `think: false`: a thinking model (qwen3, the model `config set
        // model ollama` chooses) otherwise spends the output budget on its
        // reasoning before the schema-constrained answer, and the reply ends
        // at `length` with no answer. Models without thinking accept it.
        "ollama" => (
            "api/chat".to_string(),
            json!({"model":p.model,"messages":[{"role":"system","content":instructions},{"role":"user","content":user}],
            "stream":false,"think":false,"format":schema,"options":{"num_predict":output_limit,"num_ctx":p.input_limit+p.output_limit}}),
        ),
        "anthropic" => (
            "messages".to_string(),
            json!({"model":p.model,"system":format!("{instructions}\n{ANTHROPIC_RESULT_TOOL}"),"messages":[{"role":"user","content":user}],"max_tokens":output_limit,"tools":[{"name":"graphicle_result","description":"Return the structured result for this request.","input_schema":schema}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}),
        ),
        "gemini" => (
            gemini_suffix,
            json!({"systemInstruction":{"parts":[{"text":instructions}]},"contents":[{"role":"user","parts":[{"text":user}]}],"generationConfig":{"maxOutputTokens":output_limit,"responseMimeType":"application/json","responseJsonSchema":schema}}),
        ),
        _ => return Err(Error::bad("Unsupported inference protocol")),
    };
    if !p.structured_output {
        let prompt = format!(
            "{instructions}\nReturn only one JSON object, without markdown fences, matching this JSON schema exactly:\n{full}"
        );
        match p.protocol.as_str() {
            "responses" | "azure_responses" => {
                body["instructions"] = json!(prompt);
                body.as_object_mut().unwrap().remove("text");
            }
            "chat" | "azure_chat" => {
                body["messages"][0]["content"] = json!(prompt);
                body.as_object_mut().unwrap().remove("response_format");
            }
            "anthropic" => {
                body["system"] = json!(prompt);
                body.as_object_mut().unwrap().remove("tools");
                body.as_object_mut().unwrap().remove("tool_choice");
            }
            "gemini" => {
                body["systemInstruction"]["parts"][0]["text"] = json!(prompt);
                body["generationConfig"]
                    .as_object_mut()
                    .unwrap()
                    .remove("responseJsonSchema");
            }
            _ => {
                body["messages"][0]["content"] = json!(prompt);
                body.as_object_mut().unwrap().remove("format");
            }
        }
    }
    if let Some(t) = p.temperature {
        if p.protocol == "ollama" {
            body["options"]["temperature"] = json!(t);
        } else if p.protocol == "gemini" {
            body["generationConfig"]["temperature"] = json!(t);
        } else {
            body["temperature"] = json!(t);
        }
    }
    if let Some(extra) = &p.extra_body
        && matches!(p.protocol.as_str(), "chat" | "azure_chat")
    {
        for (name, value) in extra {
            if !OWNED_CHAT_FIELDS.contains(&name.as_str()) {
                body[name] = value.clone();
            }
        }
    }
    Ok((suffix, body))
}

/// A provider's reply `value` to [`request_body`] as a [`Reply`]: the answer
/// text, with the provider's usage counts when it reports them (else the
/// request's `estimated` input tokens). A reply that was cut off, refused
/// or empty is an error, with the provider's usage when it reported any.
pub fn reply(p: &Profile, value: &Value, estimated: usize, start: Instant) -> Result<Reply> {
    let (text, input_count, output_count) = match p.protocol.as_str() {
        "responses" | "azure_responses" => {
            if value["status"] != "completed" {
                let reason = match value["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "max_output_tokens",
                    Some("content_filter") => "content_filter",
                    _ => "reason_unavailable",
                };
                let mut error = Error::bad(format!(
                    "Provider response was incomplete ({reason}); no memory was committed"
                ));
                error.provider_failure = Some(Failure {
                    reason: reason.into(),
                    input_tokens: value["usage"]["input_tokens"].as_u64(),
                    output_tokens: value["usage"]["output_tokens"].as_u64(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    model: value["model"].as_str().unwrap_or(&p.model).to_string(),
                });
                return Err(error);
            }
            let mut text = String::new();
            for item in value["output"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing response output"))?
            {
                if let Some(content) = item["content"].as_array() {
                    for block in content {
                        if block["type"] == "refusal" {
                            return Err(Error::bad("Model declined the inference request"));
                        }
                        if block["type"] == "output_text" {
                            text.push_str(block["text"].as_str().unwrap_or(""));
                        }
                    }
                }
            }
            (
                text,
                value["usage"]["input_tokens"].as_u64(),
                value["usage"]["output_tokens"].as_u64(),
            )
        }
        "chat" | "azure_chat" => {
            let choice = &value["choices"][0];
            if choice["finish_reason"] != "stop"
                || choice["message"]["refusal"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            {
                let reason = if choice["message"]["refusal"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                {
                    "refusal"
                } else {
                    match choice["finish_reason"].as_str() {
                        Some("length") => "max_output_tokens",
                        Some("content_filter") => "content_filter",
                        _ => "reason_unavailable",
                    }
                };
                let mut error = Error::bad(format!(
                    "Model did not complete a structured answer ({reason}); no memory was committed"
                ));
                error.provider_failure = Some(Failure {
                    reason: reason.into(),
                    input_tokens: value["usage"]["prompt_tokens"].as_u64(),
                    output_tokens: value["usage"]["completion_tokens"].as_u64(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    model: value["model"].as_str().unwrap_or(&p.model).to_string(),
                });
                return Err(error);
            }
            (
                choice["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                value["usage"]["prompt_tokens"].as_u64(),
                value["usage"]["completion_tokens"].as_u64(),
            )
        }
        "anthropic" => {
            let usage = &value["usage"];
            let input = usage["input_tokens"].as_u64().map(|v| {
                v + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                    + usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
            });
            // A refusal (HTTP 200, `stop_reason: refusal`) or a reply cut at
            // `max_tokens` is reported as such, with the provider's usage.
            let cut = match value["stop_reason"].as_str() {
                Some("refusal") => Some((
                    "refusal",
                    "Model declined the inference request; no memory was committed",
                )),
                Some("max_tokens") => Some((
                    "max_output_tokens",
                    "Model did not complete a structured answer (max_output_tokens); no memory was committed",
                )),
                _ => None,
            };
            if let Some((reason, message)) = cut {
                let mut error = Error::bad(message);
                error.provider_failure = Some(Failure {
                    reason: reason.into(),
                    input_tokens: input,
                    output_tokens: usage["output_tokens"].as_u64(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    model: value["model"].as_str().unwrap_or(&p.model).to_string(),
                });
                return Err(error);
            }
            let blocks = value["content"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing model output"))?;
            // Thinking blocks (always on for Claude Opus 5.5) are not part
            // of the answer.
            let written = blocks
                .iter()
                .filter(|v| v["type"] == "text")
                .filter_map(|v| v["text"].as_str())
                .collect::<String>();
            let tools = blocks
                .iter()
                .filter(|v| v["type"] == "tool_use")
                .collect::<Vec<_>>();
            let text = if p.structured_output {
                match tools.as_slice() {
                    [tool]
                        if tool["name"] == "graphicle_result"
                            && value["stop_reason"] == "tool_use" =>
                    {
                        tool["input"].to_string()
                    }
                    // `tool_choice: auto` lets the model write the result
                    // as text; it is validated against the schema like any
                    // other reply.
                    [] if value["stop_reason"] == "end_turn" && !written.trim().is_empty() => {
                        written
                    }
                    [] => {
                        return Err(Error::bad("Model did not complete structured extraction"));
                    }
                    _ => return Err(Error::bad("Unexpected structured result tool")),
                }
            } else {
                if value["stop_reason"] != "end_turn" {
                    return Err(Error::bad("Model answer was incomplete"));
                }
                written
            };
            (text, input, usage["output_tokens"].as_u64())
        }
        "gemini" => {
            let candidate = &value["candidates"][0];
            if candidate["finishReason"] != "STOP" {
                return Err(Error::bad("Model answer was blocked or incomplete"));
            }
            let text = candidate["content"]["parts"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing model output"))?
                .iter()
                .filter(|v| v["thought"] != true)
                .filter_map(|v| v["text"].as_str())
                .collect::<String>();
            let usage = &value["usageMetadata"];
            (
                text,
                usage["promptTokenCount"].as_u64(),
                usage["candidatesTokenCount"]
                    .as_u64()
                    .map(|v| v + usage["thoughtsTokenCount"].as_u64().unwrap_or(0)),
            )
        }
        _ => {
            if value["done_reason"] == "length" {
                let mut error = Error::bad(
                    "Local model response was incomplete (max_output_tokens); no memory was committed",
                );
                error.provider_failure = Some(Failure {
                    reason: "max_output_tokens".into(),
                    input_tokens: value["prompt_eval_count"].as_u64(),
                    output_tokens: value["eval_count"].as_u64(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    model: value["model"].as_str().unwrap_or(&p.model).to_string(),
                });
                return Err(error);
            }
            if value["done"] != true {
                return Err(Error::bad("Local model response was incomplete"));
            }
            (
                value["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                value["prompt_eval_count"].as_u64(),
                value["eval_count"].as_u64(),
            )
        }
    };
    if text.trim().is_empty() {
        return Err(Error::bad("Model returned no answer"));
    }
    Ok(Reply {
        text: text.clone(),
        input_tokens: input_count.map(|n| n as usize).unwrap_or(estimated),
        output_tokens: output_count
            .map(|n| n as usize)
            .unwrap_or_else(|| graphicle_memory::text::count(&text)),
        usage_reported: input_count.is_some() && output_count.is_some(),
        elapsed_ms: start.elapsed().as_millis() as u64,
        model: value["model"].as_str().unwrap_or(&p.model).to_string(),
    })
}
pub async fn embed(
    client: &reqwest::Client,
    p: &Profile,
    texts: &[String],
) -> Result<(Vec<Vec<f64>>, usize, bool)> {
    embed_with_credential(client, p, texts, environment_credential(p)?.as_deref()).await
}
pub async fn embed_with_credential(
    client: &reqwest::Client,
    p: &Profile,
    texts: &[String],
    credential: Option<&str>,
) -> Result<(Vec<Vec<f64>>, usize, bool)> {
    embed_role(client, p, texts, credential, false).await
}
/// `query` selects the retrieval-question role; only local manifests with a
/// `query_prefix` treat it differently from indexed passages.
pub async fn embed_role(
    client: &reqwest::Client,
    p: &Profile,
    texts: &[String],
    credential: Option<&str>,
    query: bool,
) -> Result<(Vec<Vec<f64>>, usize, bool)> {
    if texts.is_empty() || texts.len() > 32 || texts.iter().any(|s| s.len() > 32000) {
        return Err(Error::bad("Embed 1–32 bounded texts"));
    }
    let tokens = texts
        .iter()
        .map(|s| graphicle_memory::text::count(s))
        .sum::<usize>();
    if tokens > p.input_limit {
        return Err(Error::bad(
            "Embedding request exceeds the model input budget",
        ));
    }
    if p.protocol == "local_onnx" {
        let profile = p.clone();
        let texts = texts.to_vec();
        return tokio::task::spawn_blocking(move || {
            if query {
                crate::local_inference::embed_query(&profile, &texts)
            } else {
                crate::local_inference::embed(&profile, &texts)
            }
        })
        .await
        .map_err(|_| Error::bad("Local embedding worker failed"))?;
    }
    if p.protocol == "anthropic" {
        return Err(Error::bad(
            "This provider has no embedding endpoint; configure a separate embedding model",
        ));
    }
    // Adapted from MemOS CachingEmbedder.embed: deduplicate exact texts within
    // this request, then restore caller order. Borrow keys and keep no shared
    // cache, so provider/model/credential and workspace scopes cannot mix.
    let mut by_text = BTreeMap::<&str, usize>::new();
    let mut unique = Vec::with_capacity(texts.len());
    let mut order = Vec::with_capacity(texts.len());
    let mut uses = Vec::<usize>::new();
    for text in texts {
        let index = *by_text.entry(text.as_str()).or_insert_with(|| {
            let index = unique.len();
            unique.push(text);
            uses.push(0);
            index
        });
        uses[index] += 1;
        order.push(index);
    }
    let submitted_tokens = if unique.len() == texts.len() {
        tokens
    } else {
        unique
            .iter()
            .map(|text| graphicle_memory::text::count(text))
            .sum()
    };
    let gemini_suffix = format!(
        "models/{}:batchEmbedContents",
        url::form_urlencoded::byte_serialize(p.model.trim_start_matches("models/").as_bytes())
            .collect::<String>()
    );
    let (suffix, body) = if p.protocol == "gemini" {
        (
            gemini_suffix.as_str(),
            json!({"requests":unique.iter().map(|text|json!({"model":format!("models/{}",p.model.trim_start_matches("models/")),"content":{"parts":[{"text":text}]}})).collect::<Vec<_>>()}),
        )
    } else if p.protocol == "ollama" {
        (
            "api/embed",
            json!({"model":p.model,"input":unique,"truncate":false}),
        )
    } else {
        (
            "embeddings",
            json!({"model":p.model,"input":unique,"encoding_format":"float"}),
        )
    };
    let value = provider_json(
        authenticated(client, p, suffix, credential)?
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::bad("Embedding provider request failed"))?,
        credential,
    )
    .await?;
    let raw = if p.protocol == "gemini" {
        json!(
            value["embeddings"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing embedding vectors"))?
                .iter()
                .map(|v| v["values"].clone())
                .collect::<Vec<_>>()
        )
    } else if p.protocol == "ollama" {
        value["embeddings"].clone()
    } else {
        let mut data = value["data"]
            .as_array()
            .cloned()
            .ok_or_else(|| Error::bad("Missing embedding vectors"))?;
        data.sort_by_key(|v| v["index"].as_u64().unwrap_or(u64::MAX));
        if data
            .iter()
            .enumerate()
            .any(|(i, v)| v["index"].as_u64() != Some(i as u64))
        {
            return Err(Error::bad("Embedding indexes do not match inputs"));
        }
        json!(
            data.iter()
                .map(|v| v["embedding"].clone())
                .collect::<Vec<_>>()
        )
    };
    let mut vectors: Vec<Vec<f64>> = serde_json::from_value(raw)?;
    let dimensions = vectors.first().map_or(0, Vec::len);
    if vectors.len() != unique.len()
        || !(1..=8192).contains(&dimensions)
        || vectors
            .iter()
            .any(|v| v.len() != dimensions || v.iter().any(|x| !x.is_finite() || x.abs() > 1e6))
    {
        return Err(Error::bad("Invalid embedding dimensions or values"));
    }
    let reported = value["usage"]["prompt_tokens"]
        .as_u64()
        .or(value["prompt_eval_count"].as_u64());
    let vectors = if unique.len() == texts.len() {
        vectors
    } else {
        order
            .into_iter()
            .map(|index| {
                uses[index] -= 1;
                if uses[index] == 0 {
                    std::mem::take(&mut vectors[index])
                } else {
                    vectors[index].clone()
                }
            })
            .collect()
    };
    Ok((
        vectors,
        reported.map(|v| v as usize).unwrap_or(submitted_tokens),
        reported.is_some(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        Config, Profile, checked_url, http_hosts, provider_url_with, refusal_message, reply,
        request_body,
    };
    use serde_json::{Value, json};
    use std::time::Instant;

    fn profile(protocol: &str, structured: bool) -> Profile {
        serde_json::from_value(
            json!({"protocol":protocol,"base_url":"http://127.0.0.1:9","model":"m",
            "input_limit":4000,"output_limit":1000,"structured_output":structured}),
        )
        .unwrap()
    }

    /// A reflect step's schema: string-length keywords and an enum.
    fn schema() -> Value {
        json!({"type":"object","properties":{"action":{"enum":["answer","search"]},
            "queries":{"type":"array","maxItems":4,"items":{"type":"string","minLength":1,"maxLength":2000}},
            "answer":{"type":"string"}}})
    }

    /// The request each protocol sends: Ollama turns thinking off, the
    /// Anthropic tool is offered without forcing it, and the OpenAI-style
    /// schemas leave out the string-length keywords strict mode refuses.
    #[test]
    fn requests_take_each_providers_shape() {
        let input = json!({"question":"q"});
        let (path, ollama) =
            request_body(&profile("ollama", true), "Rules", &input, &schema(), 500).unwrap();
        assert_eq!(path, "api/chat");
        assert_eq!(ollama["think"], false);
        assert_eq!(ollama["stream"], false);
        assert_eq!(ollama["options"]["num_predict"], 500);
        assert_eq!(
            ollama["format"]["properties"]["queries"]["items"]["minLength"],
            1
        );
        let (_, plain) =
            request_body(&profile("ollama", false), "Rules", &input, &schema(), 500).unwrap();
        assert_eq!(plain["think"], false);
        assert!(plain.get("format").is_none());

        let (path, anthropic) =
            request_body(&profile("anthropic", true), "Rules", &input, &schema(), 500).unwrap();
        assert_eq!(path, "messages");
        assert_eq!(
            anthropic["tool_choice"],
            json!({"type":"auto","disable_parallel_tool_use":true})
        );
        assert_eq!(anthropic["tools"][0]["name"], "graphicle_result");
        assert!(anthropic["system"].as_str().unwrap().starts_with("Rules\n"));
        assert!(
            anthropic["system"]
                .as_str()
                .unwrap()
                .contains("graphicle_result tool")
        );
        let (_, prompted) = request_body(
            &profile("anthropic", false),
            "Rules",
            &input,
            &schema(),
            500,
        )
        .unwrap();
        assert!(prompted.get("tools").is_none() && prompted.get("tool_choice").is_none());
        assert!(
            prompted["system"]
                .as_str()
                .unwrap()
                .contains("matching this JSON schema")
        );

        for protocol in ["responses", "chat"] {
            let (_, body) =
                request_body(&profile(protocol, true), "Rules", &input, &schema(), 500).unwrap();
            let text = body.to_string();
            assert!(
                !text.contains("minLength") && !text.contains("maxLength"),
                "{protocol}: {text}"
            );
            assert!(
                text.contains("\"maxItems\":4") && text.contains("\"strict\":true"),
                "{protocol}: {text}"
            );
            // A prompt-only profile still shows the whole schema.
            let (_, prompted) =
                request_body(&profile(protocol, false), "Rules", &input, &schema(), 500).unwrap();
            assert!(prompted.to_string().contains("minLength"), "{protocol}");
        }
        let (_, gemini) =
            request_body(&profile("gemini", true), "Rules", &input, &schema(), 500).unwrap();
        assert!(gemini.to_string().contains("minLength"));
    }

    /// Replies: Claude's thinking blocks are not part of the answer; a
    /// refusal or a cut-off reply says so and keeps the provider's usage; an
    /// Ollama reply cut at its output limit names that limit.
    #[test]
    fn replies_report_refusals_and_cut_off_answers() {
        let start = Instant::now();
        let prompted = profile("anthropic", false);
        let thought = json!({"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":7},
            "content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"answer\":\"ready\"}"}]});
        assert_eq!(
            reply(&prompted, &thought, 99, start).unwrap().text,
            "{\"answer\":\"ready\"}"
        );
        let structured = profile("anthropic", true);
        let tool = json!({"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":7},
            "content":[{"type":"thinking","thinking":""},{"type":"tool_use","name":"graphicle_result","input":{"answer":"ready"}}]});
        assert_eq!(
            reply(&structured, &tool, 99, start)
                .unwrap()
                .json()
                .unwrap()["answer"],
            "ready"
        );
        // tool_choice auto: a result written as text is read like any reply.
        assert_eq!(
            reply(&structured, &thought, 99, start)
                .unwrap()
                .json()
                .unwrap()["answer"],
            "ready"
        );
        let nothing = json!({"stop_reason":"end_turn","content":[{"type":"text","text":" "}]});
        assert!(reply(&structured, &nothing, 99, start).is_err());
        for (stop, words, reason) in [
            ("refusal", "declined", "refusal"),
            ("max_tokens", "(max_output_tokens)", "max_output_tokens"),
        ] {
            let cut = json!({"stop_reason":stop,"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":7},"content":[]});
            for p in [&prompted, &structured] {
                let error = reply(p, &cut, 99, start).unwrap_err();
                assert!(error.message.contains(words), "{stop}: {}", error.message);
                let failure = error.provider_failure.unwrap();
                assert_eq!(
                    (
                        failure.reason.as_str(),
                        failure.input_tokens,
                        failure.output_tokens
                    ),
                    (reason, Some(15), Some(7))
                );
            }
        }
        let ollama = profile("ollama", true);
        let length = json!({"done":true,"done_reason":"length","message":{"content":"{"},"prompt_eval_count":12,"eval_count":1000});
        let error = reply(&ollama, &length, 99, start).unwrap_err();
        assert!(
            error
                .message
                .starts_with("Local model response was incomplete (max_output_tokens)"),
            "{}",
            error.message
        );
        assert_eq!(error.provider_failure.unwrap().output_tokens, Some(1000));
        let done = json!({"done":true,"done_reason":"stop","message":{"content":"{\"answer\":\"ready\"}"},"prompt_eval_count":12,"eval_count":5});
        let ok = reply(&ollama, &done, 99, start).unwrap();
        assert!(ok.usage_reported && ok.input_tokens == 12 && ok.output_tokens == 5);
    }

    /// A refused request carries the provider's reason, on one line, short,
    /// and never with the key.
    #[test]
    fn refusals_keep_the_providers_reason_without_the_key() {
        let openai = br#"{"error":{"message":"Incorrect API key provided: sk-test-key. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error"}}"#;
        let message = refusal_message(401, openai, Some("sk-test-key"));
        assert_eq!(
            message,
            "Provider returned HTTP 401: Incorrect API key provided: [redacted] You can find your API key at https://platform.openai.com/account/api-keys."
        );
        assert_eq!(
            refusal_message(404, br#"{"error":"model 'qwen3:8b' not found"}"#, None),
            "Provider returned HTTP 404: model 'qwen3:8b' not found"
        );
        assert_eq!(
            refusal_message(
                400,
                br#"{"object":"error","message":"This model's maximum context length is 4096 tokens.\nPlease reduce the length.","type":"BadRequestError"}"#,
                None
            ),
            "Provider returned HTTP 400: This model's maximum context length is 4096 tokens. Please reduce the length."
        );
        assert!(
            refusal_message(
                422,
                br#"{"detail":[{"loc":["body","model"],"msg":"field required"}]}"#,
                None
            )
            .contains("field required")
        );
        assert_eq!(
            refusal_message(502, b"<html><body>Bad gateway</body></html>", None),
            "Provider returned HTTP 502"
        );
        assert_eq!(
            refusal_message(500, b"", None),
            "Provider returned HTTP 500"
        );
        assert_eq!(
            refusal_message(
                503,
                b"upstream busy, secret-token-here",
                Some("secret-token-here")
            ),
            "Provider returned HTTP 503: upstream busy, [redacted]"
        );
        let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(1000));
        let message = refusal_message(400, long.as_bytes(), None);
        assert!(message.ends_with("...") && message.len() < 340, "{message}");
    }

    /// `worker_concurrency` takes 1 to 64 workers, the range `max_model_jobs`
    /// takes; one worker stays the default.
    #[test]
    fn worker_concurrency_takes_one_to_sixty_four_and_defaults_to_one() {
        let workers = |n: usize| Config {
            worker_concurrency: n,
            ..Config::default()
        };
        for n in [1, 4, 5, 32, 64] {
            assert!(workers(n).validate().is_ok(), "{n}");
        }
        for n in [0, 65] {
            let refused = workers(n).validate().unwrap_err();
            assert!(
                refused.message.contains("worker_concurrency 1 to 64"),
                "{n}: {}",
                refused.message
            );
        }
        assert_eq!(Config::default().worker_concurrency, 1);
        assert_eq!(Config::load(None).unwrap().worker_concurrency, 1);
        let parsed: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.worker_concurrency, 1);
    }

    /// P-URL (EVAL_PLAN R5): a model profile reaches a plain-HTTP host only
    /// when `GRAPHICLE_PROVIDER_HTTP_HOSTS` lists that exact host, and never
    /// with a credential; HTTPS, loopback and every other URL rule stay.
    #[test]
    fn plain_http_model_hosts_are_exact_opt_ins_without_credentials() {
        let hosts = http_hosts("192.168.210.33, vllm.internal\n[fd00::7]").unwrap();
        assert_eq!(hosts, ["192.168.210.33", "vllm.internal", "[fd00::7]"]);
        for url in [
            "http://192.168.210.33:8000/v1",
            "http://vllm.internal/v1",
            "http://[fd00::7]:9000/v1",
        ] {
            assert!(provider_url_with(url, false, &hosts).is_ok(), "{url}");
            let refused = provider_url_with(url, true, &hosts).unwrap_err();
            assert!(
                refused.message.contains("never sent over plain HTTP"),
                "{url}: {}",
                refused.message
            );
        }
        // Exact hosts: no neighbour, sub- or superdomain, and no other address.
        for url in [
            "http://192.168.210.34:8000/v1",
            "http://192.168.210.3/v1",
            "http://api.vllm.internal/v1",
            "http://vllm.internal.example.com/v1",
            "http://internal/v1",
            "http://[fd00::8]/v1",
        ] {
            let refused = provider_url_with(url, false, &hosts).unwrap_err();
            assert!(
                refused.message.contains("GRAPHICLE_PROVIDER_HTTP_HOSTS"),
                "{url}: {}",
                refused.message
            );
        }
        // Without the list, plain HTTP off loopback is refused as before;
        // HTTPS and loopback HTTP keep working, with or without a credential.
        assert!(provider_url_with("http://192.168.210.33/v1", false, &[]).is_err());
        assert!(provider_url_with("https://api.example.com/v1", true, &[]).is_ok());
        assert!(provider_url_with("http://127.0.0.1:11434", true, &[]).is_ok());
        // A listed host still takes no user, query or fragment in its URL.
        for url in [
            "http://user@vllm.internal/v1",
            "http://vllm.internal/v1?key=x",
            "http://vllm.internal/v1#part",
        ] {
            assert!(provider_url_with(url, false, &hosts).is_err(), "{url}");
        }
        // Connectors and OAuth use the strict rule, whatever the list says.
        assert!(checked_url("http://192.168.210.33:8000/v1").is_err());
    }

    #[test]
    fn the_plain_http_host_list_takes_exact_hosts_only() {
        assert!(http_hosts("").unwrap().is_empty());
        assert!(http_hosts(" , ").unwrap().is_empty());
        for entry in [
            "vllm.internal:8000",
            "http://vllm.internal",
            "vllm.internal/v1",
            "*.internal",
            "user@vllm.internal",
            "VLLM.internal",
            "192.168.010.033",
            "fd00::7",
        ] {
            let refused = http_hosts(entry).unwrap_err();
            assert!(
                refused.message.contains("GRAPHICLE_PROVIDER_HTTP_HOSTS"),
                "{entry}: {}",
                refused.message
            );
        }
    }
}
