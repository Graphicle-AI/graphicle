// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Native HTTP, CLI and AWS transports for llm.py.

#[derive(Clone, Copy)]
pub struct CallOptions<'a> {
    pub backend: &'a str,
    pub model: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub user: &'a str,
    pub images: &'a [ImageRef],
    pub max_tokens: i64,
    pub extract: Option<bool>,
}
use crate::{
    build::Diagnostics,
    llm::{Error, Result},
    llm_config::{self as cfg, Configuration, Environment},
    llm_parse,
    llm_source::{self as source, ImageRef},
    python_compat::repr_string,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    process::Command,
    sync::Mutex,
    time::{Duration, SystemTime},
};
#[derive(Clone, Serialize)]
pub struct Request {
    pub backend: String,
    pub model: String,
    pub url: String,
    pub body: Value,
    pub timeout: f64,
    pub retries: i64,
    pub cli_args: Vec<String>,
    pub cli_input: String,
    pub cli_schema: bool,
    #[serde(skip)]
    pub(crate) headers: Vec<(String, String)>,
    #[serde(skip)]
    pub(crate) env: Environment,
    /// Ollama's native `/api/chat` request, tried before this one (0.9.91,
    /// #4413): the OpenAI-compatible `/v1` endpoint ignores `options`, so
    /// `num_ctx` only reaches the server through the native endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_native: Option<Box<Request>>,
}
/// Ollama roots that answered `/api/chat` with 404/405: servers that only
/// speak `/v1` (LM Studio, llama.cpp). Later requests skip the native call.
static OLLAMA_NO_NATIVE: std::sync::LazyLock<Mutex<BTreeSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeSet::new()));
/// `request` through `transport`, first as Ollama's native `/api/chat` call
/// when the plan has one (upstream `_call_ollama_native`). The native answer
/// is returned in the OpenAI-compatible shape the response readers expect.
/// A 404/405 that does not name the model marks the server as `/v1`-only,
/// warns once, and falls back to the `/v1` request.
pub fn send(transport: &dyn Transport, request: &Request, d: &mut Diagnostics) -> Result<Value> {
    if let Some(native) = &request.ollama_native {
        let root = native
            .url
            .strip_suffix("/api/chat")
            .unwrap_or(&native.url)
            .to_owned();
        let known = OLLAMA_NO_NATIVE
            .lock()
            .map_err(|_| Error::runtime("ollama endpoint cache lock poisoned"))?
            .contains(&root);
        if !known {
            match transport.send(native) {
                Ok(data) => return Ok(ollama_native_response(&data)),
                Err(e)
                    if matches!(e.http_status, Some(404 | 405))
                        && !e.message.to_lowercase().contains("model") =>
                {
                    OLLAMA_NO_NATIVE
                        .lock()
                        .map_err(|_| Error::runtime("ollama endpoint cache lock poisoned"))?
                        .insert(root.clone());
                    d.stderr.push(format!("[graphicle] {root}/api/chat is not available; falling back to the OpenAI-compatible /v1 endpoint, which ignores num_ctx."));
                }
                Err(e) => return Err(e),
            }
        }
    }
    transport.send(request)
}
/// An `/api/chat` reply as an OpenAI chat completion: the message content,
/// `prompt_eval_count`/`eval_count` as usage, and `done_reason == "length"`
/// as the finish reason.
pub fn ollama_native_response(data: &Value) -> Value {
    let count = |key: &str| {
        data.get(key)
            .filter(|v| crate::build::truthy(v))
            .cloned()
            .unwrap_or(json!(0))
    };
    let content = data
        .get("message")
        .filter(|m| crate::build::truthy(m))
        .and_then(|m| m.get("content"))
        .cloned()
        .unwrap_or(Value::Null);
    let finish = if data.get("done_reason").and_then(Value::as_str) == Some("length") {
        "length"
    } else {
        "stop"
    };
    json!({
        "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": finish}],
        "usage": {"prompt_tokens": count("prompt_eval_count"), "completion_tokens": count("eval_count")},
    })
}
/// `_ollama_request_extras`: `num_ctx` sized to the prompt (the
/// GRAPHICLE_OLLAMA_NUM_CTX setting wins, with warnings for a bad or too
/// small value) and `keep_alive`.
fn ollama_request_extras(
    env: &Environment,
    user: &str,
    max_tokens: i64,
    d: &mut Diagnostics,
) -> (i64, Value) {
    let input = user.chars().count() as i64 / 4 + 400;
    let auto = (input + max_tokens + 2000).clamp(8192, 131072);
    let raw = cfg::trim(cfg::get(env, "GRAPHICLE_OLLAMA_NUM_CTX"));
    let n = if raw.is_empty() {
        auto
    } else if let Some(n) = cfg::parse_integer(raw) {
        if n < input {
            d.stderr.push(format!("[graphicle] warning: GRAPHICLE_OLLAMA_NUM_CTX={n} is smaller than the estimated chunk input (~{input} tokens). Ollama will silently truncate the prompt and return empty responses. Try --token-budget {} or increase NUM_CTX.",1024.max(n.div_euclid(3))))
        }
        n
    } else {
        d.stderr.push(format!(
            "[graphicle] GRAPHICLE_OLLAMA_NUM_CTX={} is not a valid integer; using auto-derived value ({auto}).",
            repr_string(raw)
        ));
        auto
    };
    let keep_alive = json!(
        env.get("GRAPHICLE_OLLAMA_KEEP_ALIVE")
            .map(String::as_str)
            .unwrap_or("30m")
    );
    (n, keep_alive)
}
pub trait Transport: Send + Sync {
    fn send(&self, request: &Request) -> Result<Value>;
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
}
pub fn plan(
    config: &Configuration,
    context: CallOptions<'_>,
    d: &mut Diagnostics,
) -> Result<Request> {
    let CallOptions {
        backend,
        model,
        api_key,
        user,
        images,
        max_tokens,
        extract,
    } = context;

    let c = config.backend(backend).map_err(Error::value)?;
    let env = &config.env;
    // A plain call validates the seed before anything else; an extraction
    // validates it with the request (as `_call_openai_compat` does).
    let mut ollama_seed = if backend == "ollama" && extract.is_none() {
        cfg::resolve_ollama_seed(env).map_err(Error::value)?
    } else {
        None
    };
    let explicit_model = model.is_some();
    let model = match model.filter(|s| !s.is_empty()) {
        Some(m) => m.into(),
        None => config.model(backend).map_err(Error::value)?,
    };
    let mut key = api_key
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(config.api_key(backend).map_err(Error::value)?);
    if key.is_empty() && backend == "ollama" {
        let url = cfg::resolve_ollama_base_url(env, c["base_url"].as_str().unwrap_or(""));
        cfg::validate_ollama_url_with(&url, true, cfg::metadata_host, d).map_err(Error::value)?;
        if extract.is_some() {
            d.stderr.push(format!("[graphicle] WARNING: ollama backend selected with no OLLAMA_API_KEY set; sending corpus to {url}. Set OLLAMA_API_KEY (any non-empty value) to suppress this warning."))
        }
        key = "ollama".into();
    }
    if key.is_empty() && !["bedrock", "claude-cli"].contains(&backend) {
        return Err(Error::value(format!(
            "No API key for backend '{backend}'. Set {}{}",
            config.format_env_keys(backend).map_err(Error::value)?,
            if extract.is_some() {
                " or pass api_key=."
            } else {
                "."
            }
        )));
    }
    let mut request = Request {
        backend: backend.into(),
        model: model.clone(),
        url: String::new(),
        body: Value::Null,
        timeout: cfg::timeout(env, 600.0),
        retries: cfg::max_retries(env, 6),
        headers: vec![],
        env: env.clone(),
        cli_args: vec![],
        cli_input: String::new(),
        cli_schema: false,
        ollama_native: None,
    };
    let system = extract.map(crate::llm::extraction_system);
    match backend {
        "claude-cli" => {
            request.cli_args = vec![
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--no-session-persistence".into(),
            ];
            if let Some(system) = system {
                let mut dirs = BTreeSet::new();
                for r in images {
                    let dir = r
                        .path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .to_string_lossy()
                        .into_owned();
                    if dirs.insert(dir.clone()) {
                        request.cli_args.extend(["--add-dir".into(), dir]);
                    }
                }
                let cli_model = cfg::trim(cfg::get(env, "GRAPHICLE_CLAUDE_CLI_MODEL"));
                if !cli_model.is_empty() {
                    request
                        .cli_args
                        .extend(["--model".into(), cli_model.into()])
                }
                request.cli_schema = true;
                request.cli_input = format!(
                    "{system}\n\n---\nNow extract the knowledge graph from the following source file(s) and output ONLY the JSON object described above. No prose, no preamble, no markdown fences.\n\n{}",
                    source::with_image_notes(user, images, true)
                );
            } else {
                // An explicit model wins; otherwise GRAPHICLE_CLAUDE_CLI_MODEL
                // applies to plain calls too (Graphify 0.9.87, #3846).
                let cli_model = if explicit_model {
                    model.clone()
                } else {
                    cfg::trim(cfg::get(env, "GRAPHICLE_CLAUDE_CLI_MODEL")).to_string()
                };
                if !cli_model.is_empty() {
                    request.cli_args.extend(["--model".into(), cli_model])
                }
                request.cli_input = user.into();
            }
        }
        "bedrock" => {
            request.body = json!({"modelId":model,"messages":[{"role":"user","content":if extract.is_some(){source::bedrock_content(user,images)}else{json!([{"text":user}])}}],"inferenceConfig":cfg::bedrock_inference(env,max_tokens,&model,d)});
            if let Some(s) = system {
                request.body["system"] = json!([{"text":s}])
            }
            request.retries = request.retries.saturating_add(1);
        }
        "claude" => {
            request.url = format!(
                "{}/v1/messages",
                c["base_url"]
                    .as_str()
                    .unwrap_or("https://api.anthropic.com")
                    .trim_end_matches('/')
            );
            request.headers = vec![
                ("x-api-key".into(), key),
                ("anthropic-version".into(), "2023-06-01".into()),
            ];
            request.body = json!({"model":model,"max_tokens":max_tokens,"messages":[{"role":"user","content":if extract.is_some(){source::anthropic_content(user,images)}else{json!(user)}}]});
            if let Some(s) = system {
                request.body["system"] = json!(s)
            }
        }
        _ => {
            let base = c["base_url"].as_str().unwrap_or("");
            let mut messages = vec![];
            if let Some(s) = system {
                messages.push(json!({"role":"system","content":s}))
            }
            messages.push(json!({"role":"user","content":if extract.is_some()&&backend!="azure"{source::openai_content(user,images)}else{json!(user)}}));
            request.body =
                json!({"model":model,"messages":messages,"max_completion_tokens":max_tokens});
            let default = c
                .get("temperature")
                .map(|v| v.as_f64())
                .unwrap_or(Some(0.0));
            if let Some(t) = cfg::temperature(env, default, &model, d) {
                request.body["temperature"] = json!(t)
            }
            if backend == "azure" {
                let endpoint = cfg::trim(cfg::get(env, "AZURE_OPENAI_ENDPOINT"));
                if endpoint.is_empty() {
                    return Err(Error::value(if extract.is_some() {
                        "Azure OpenAI backend requires AZURE_OPENAI_ENDPOINT to be set (e.g. https://my-resource.openai.azure.com/)."
                    } else {
                        "Azure OpenAI backend requires AZURE_OPENAI_ENDPOINT to be set."
                    }));
                }
                let mut url = reqwest::Url::parse(&format!(
                    "{}/openai/deployments/",
                    endpoint.trim_end_matches('/')
                ))
                .map_err(|e| Error::value(e.to_string()))?;
                url.path_segments_mut()
                    .map_err(|_| Error::value("Invalid Azure endpoint"))?
                    .pop_if_empty()
                    .push(&model)
                    .push("chat")
                    .push("completions");
                url.query_pairs_mut().append_pair(
                    "api-version",
                    cfg::trim(
                        env.get("AZURE_OPENAI_API_VERSION")
                            .map(String::as_str)
                            .unwrap_or("2024-12-01-preview"),
                    ),
                );
                request.url = url.to_string();
                request.headers = vec![("api-key".into(), key)];
            } else {
                request.url = format!("{}/chat/completions", base.trim_end_matches('/'));
                request.headers = vec![("authorization".into(), format!("Bearer {key}"))];
                request.body["stream"] = json!(false);
                if let Some(v) = c.get("reasoning_effort").filter(|v| {
                    !v.is_null() && (extract.is_some() || crate::exporters::common::truth(v))
                }) {
                    request.body["reasoning_effort"] = v.clone()
                }
                if backend == "ollama" && extract.is_some() {
                    ollama_seed = cfg::resolve_ollama_seed(env).map_err(Error::value)?;
                }
                // Opt-in reproducibility for built-in Ollama (#4168).
                if let Some(seed) = ollama_seed {
                    request.body["seed"] = json!(seed)
                }
                let explicit = c.get("extra_body").filter(|v| !v.is_null());
                let mut extra = explicit.cloned().or_else(|| {
                    if base.contains("moonshot") || cfg::yes(env, "GRAPHICLE_DISABLE_THINKING") {
                        Some(json!({"thinking":{"type":"disabled"}}))
                    } else {
                        None
                    }
                });
                if backend == "ollama"
                    && extract.is_some()
                    && cfg::trim(cfg::get(env, "GRAPHICLE_MAX_RETRIES")).is_empty()
                {
                    request.retries = 0
                }
                // The num_ctx default applies to extraction and, since 0.9.91
                // (#4413), to plain calls (community labels) too. A provider
                // with its own extra_body has opted out.
                let ollama = backend == "ollama" && explicit.is_none();
                if ollama {
                    let (n, keep_alive) = ollama_request_extras(env, user, max_tokens, d);
                    // Merge, don't assign: GRAPHICLE_DISABLE_THINKING may have
                    // set the body above (Graphify 69d87e0, #3988).
                    let mut merged = extra
                        .take()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default();
                    merged.insert("options".into(), json!({"num_ctx": n}));
                    merged.insert("keep_alive".into(), keep_alive);
                    extra = Some(Value::Object(merged));
                }
                if let Some(extra) = extra {
                    let Some(m) = extra.as_object() else {
                        return Err(Error::value("extra_body must be a JSON object"));
                    };
                    for (k, v) in m {
                        request.body[k] = v.clone()
                    }
                }
                if ollama {
                    request.ollama_native = Some(Box::new(ollama_native_request(
                        &request, base, user, images, max_tokens,
                    )));
                }
            }
        }
    }
    Ok(request)
}
/// `_call_ollama_native`'s request: the planned `/v1` request's model,
/// leading messages, options (with `num_predict`, and the temperature and
/// seed when set) and keep-alive, sent to `<root>/api/chat`, where root is
/// the base URL without a trailing `/v1`. Images go as base64 beside the
/// text; `think: false` when GRAPHICLE_DISABLE_THINKING is set.
fn ollama_native_request(
    request: &Request,
    base: &str,
    user: &str,
    images: &[ImageRef],
    max_tokens: i64,
) -> Request {
    let root = base.trim_end_matches('/');
    let root = root.strip_suffix("/v1").unwrap_or(root);
    let body = &request.body;
    let mut options = body["options"].as_object().cloned().unwrap_or_default();
    options.insert("num_predict".into(), json!(max_tokens));
    for key in ["temperature", "seed"] {
        if let Some(v) = body.get(key) {
            options.insert(key.into(), v.clone());
        }
    }
    let mut user_message =
        json!({"role": "user", "content": source::with_image_notes(user, images, false)});
    let pixels: Vec<String> = images
        .iter()
        .filter(|r| r.raw.is_some())
        .map(ImageRef::b64)
        .collect();
    if !pixels.is_empty() {
        user_message["images"] = json!(pixels);
    }
    let mut messages: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();
    messages.pop();
    messages.push(user_message);
    let mut native = json!({
        "model": body["model"],
        "messages": messages,
        "stream": false,
        "options": options,
        "keep_alive": body.get("keep_alive").cloned().unwrap_or(json!("30m")),
    });
    if cfg::yes(&request.env, "GRAPHICLE_DISABLE_THINKING") {
        native["think"] = json!(false);
    }
    Request {
        url: format!("{root}/api/chat"),
        body: native,
        ollama_native: None,
        ..request.clone()
    }
}
#[derive(Default)]
pub struct NativeTransport {
    schema_support: Mutex<HashMap<std::path::PathBuf, bool>>,
}
impl NativeTransport {
    fn cli(&self, r: &Request) -> Result<Value> {
        let executable=cfg::claude_command(&r.env).ok_or_else(||Error::runtime("Claude Code CLI not found on $PATH. Install from https://claude.ai/code and run `claude` once to authenticate."))?;
        let mut args = r.cli_args.clone();
        if r.cli_schema {
            let cached = self
                .schema_support
                .lock()
                .map_err(|_| Error::runtime("CLI capability cache lock poisoned"))?
                .get(&executable)
                .copied();
            let supported = if let Some(v) = cached {
                v
            } else {
                let mut command = Command::new(&executable);
                command.arg("--help").envs(&r.env);
                no_window(&mut command);
                let found = crate::process::capture(&mut command, Duration::from_secs(30))
                    .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--json-schema"));
                self.schema_support
                    .lock()
                    .map_err(|_| Error::runtime("CLI capability cache lock poisoned"))?
                    .insert(executable.clone(), found);
                found
            };
            if supported {
                args.extend([
                    "--json-schema".into(),
                    include_str!("assets/llm/extraction_schema.json").into(),
                ])
            }
        }
        let mut command = Command::new(&executable);
        command.args(&args).envs(&r.env);
        no_window(&mut command);
        let output = crate::process::capture_with_input(
            &mut command,
            duration(r.timeout)?,
            Some(r.cli_input.as_bytes().to_vec()),
        )
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut {
                Error::timeout(format!("claude -p timed out after {} seconds", r.timeout))
            } else {
                Error::runtime(e.to_string())
            }
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error = llm_parse::cli_error(&stdout);
        if !output.success {
            let detail = if !cfg::trim(&stderr).is_empty() {
                cfg::trim(&stderr)
            } else if !error.is_empty() {
                &error
            } else {
                "(no stderr, no error envelope)"
            };
            return Err(Error::runtime(format!(
                "claude -p exited {}: {}",
                output.code.unwrap_or(-1),
                source::cap(detail, 500)
            )));
        }
        if !error.is_empty() {
            return Err(Error::runtime(format!(
                "claude -p reported an error: {}",
                source::cap(&error, 500)
            )));
        }
        llm_parse::cli_envelope(&stdout).map_err(Error::runtime)
    }
    fn http(&self, r: &Request) -> Result<Value> {
        let client = reqwest::blocking::Client::builder()
            .timeout(duration(r.timeout)?)
            .connect_timeout(duration(r.timeout)?)
            .build()
            .map_err(|e| Error::transport(e.to_string()))?;
        let bytes = serde_json::to_vec(&r.body).map_err(|e| Error::value(e.to_string()))?;
        for attempt in 0..=r.retries {
            let mut request = client
                .post(&r.url)
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .body(bytes.clone());
            for (k, v) in &r.headers {
                request = request.header(k, v)
            }
            match request.send() {
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();
                    let body = match response.text() {
                        Ok(body) => body,
                        Err(e) => {
                            if attempt < r.retries {
                                self.sleep(retry_delay(&headers, attempt));
                                continue;
                            }
                            return Err(if e.is_timeout() {
                                Error::timeout(e.to_string())
                            } else {
                                Error::transport(e.to_string())
                            });
                        }
                    };
                    if status.is_success() {
                        return serde_json::from_str(&body).map_err(|e| {
                            Error::value(format!("LLM returned invalid response JSON: {e}"))
                        });
                    }
                    let forced = headers.get("x-should-retry").and_then(|v| v.to_str().ok());
                    let retry = forced == Some("true")
                        || (forced != Some("false")
                            && (matches!(status.as_u16(), 408 | 409 | 429)
                                || status.is_server_error()));
                    if attempt < r.retries && retry {
                        self.sleep(retry_delay(&headers, attempt));
                        continue;
                    }
                    return Err(Error::http(
                        status.as_u16(),
                        format!(
                            "{} API error (HTTP {}): {}",
                            r.backend,
                            status.as_u16(),
                            source::cap(&body, 4000)
                        ),
                    ));
                }
                Err(e) => {
                    if attempt < r.retries {
                        self.sleep(Duration::from_secs_f64(
                            (0.5 * 2_f64.powi(attempt.min(4) as i32)).min(8.0),
                        ));
                        continue;
                    }
                    return Err(if e.is_timeout() {
                        Error::timeout(e.to_string())
                    } else {
                        Error::transport(e.to_string())
                    });
                }
            }
        }
        Err(Error::runtime("HTTP retry budget was invalid"))
    }
}
fn duration(s: f64) -> Result<Duration> {
    Duration::try_from_secs_f64(s)
        .map_err(|_| Error::value("API timeout must be a finite positive duration"))
}
pub(crate) fn no_window(_command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        _command.creation_flags(0x08000000);
    }
}
fn retry_delay(headers: &reqwest::header::HeaderMap, attempt: i64) -> Duration {
    let mut delay = headers
        .get("retry-after-ms")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .map(|n| n / 1000.0);
    if delay.is_none()
        && let Some(s) = headers.get("retry-after").and_then(|h| h.to_str().ok())
    {
        delay = s.parse::<f64>().ok().or_else(|| {
            httpdate::parse_http_date(s)
                .ok()
                .and_then(|v| v.duration_since(SystemTime::now()).ok())
                .map(|d| d.as_secs_f64())
        })
    }
    let seconds = delay.filter(|v| *v > 0.0 && *v <= 60.0).unwrap_or_else(|| {
        (0.5 * 2_f64.powi(attempt.min(4) as i32)).min(8.0) * (1.0 - rand::random::<f64>() * 0.25)
    });
    Duration::from_secs_f64(seconds)
}
impl Transport for NativeTransport {
    fn send(&self, r: &Request) -> Result<Value> {
        match r.backend.as_str() {
            "claude-cli" => self.cli(r),
            "bedrock" => {
                #[cfg(feature = "bedrock")]
                {
                    crate::llm_bedrock::send(r)
                }
                #[cfg(not(feature = "bedrock"))]
                {
                    Err(Error::runtime(
                        "This build does not include the bedrock feature",
                    ))
                }
            }
            _ => self.http(r),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ollama_num_ctx_merges_with_disabled_thinking() {
        // Graphify 69d87e0 (#3988): num_ctx used to replace the thinking body.
        let mut env = crate::llm_config::Environment::new();
        env.insert("GRAPHICLE_DISABLE_THINKING".into(), "1".into());
        let config = Configuration::builtins(env);
        let mut d = Diagnostics::default();
        let options = CallOptions {
            backend: "ollama",
            model: Some("m"),
            api_key: None,
            user: "hello",
            images: &[],
            max_tokens: 100,
            extract: Some(false),
        };
        let request = plan(&config, options, &mut d).unwrap();
        assert_eq!(request.body["thinking"], json!({"type": "disabled"}));
        assert_eq!(request.body["options"], json!({"num_ctx": 8192}));
        assert_eq!(request.body["keep_alive"], json!("30m"));
    }
}
