// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Native model adapters, semantic extraction and community naming.
//!
//! Provider requests are injected through `Transport` for offline qualification;
//! the ordinary runtime uses native HTTP/AWS clients or the official Claude CLI.
pub use crate::llm_config::{Configuration, Environment};
pub use crate::llm_extract::{CorpusOptions, ExtractOptions, Progress};
pub use crate::llm_labels::LabelOptions;
pub use crate::llm_source::ImageRef;
pub use crate::llm_transport::{NativeTransport, Request, Transport};
use crate::{build::Diagnostics, file_slice::Unit, llm_parse as parse, llm_source as source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Value,
    Runtime,
    Timeout,
    Transport,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}
impl Error {
    pub fn value(message: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::Value,
            message: message.to_string(),
        }
    }
    pub fn runtime(message: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::Runtime,
            message: message.to_string(),
        }
    }
    pub fn transport(message: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::Transport,
            message: message.to_string(),
        }
    }
    pub fn timeout(message: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::Timeout,
            message: message.to_string(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
}
impl Usage {
    pub fn add(&mut self, u: &Self) {
        self.input += u.input;
        self.output += u.output
    }
}
pub fn extraction_system(deep: bool) -> String {
    let s = include_str!("assets/llm/extraction_system.txt");
    if deep {
        format!(
            "{s}{}",
            include_str!("assets/llm/deep_extraction_suffix.txt")
        )
    } else {
        s.into()
    }
}
#[derive(Clone)]
pub struct Runtime {
    pub config: Arc<Configuration>,
    pub transport: Arc<dyn Transport>,
    pub(crate) source: Arc<Mutex<source::TokenPacker>>,
}
impl Runtime {
    pub fn new(config: Configuration) -> Self {
        Self::with_transport(config, Arc::new(NativeTransport::default()))
    }
    pub fn from_environment(d: &mut Diagnostics) -> Self {
        Self::new(Configuration::from_environment(d))
    }
    pub fn with_transport(config: Configuration, transport: Arc<dyn Transport>) -> Self {
        Self {
            config: Arc::new(config),
            transport,
            source: Arc::new(Mutex::new(source::TokenPacker::default())),
        }
    }
    pub fn sleep(&self, seconds: f64) {
        self.transport.sleep(Duration::from_secs_f64(seconds))
    }
    /// Build a graph with this runtime available for the optional LLM tiebreak.
    pub fn build_graph(
        &mut self,
        extractions: &[Value],
        options: &crate::build::BuildOptions,
        diagnostics: &mut Diagnostics,
    ) -> crate::Result<crate::graph::Graph> {
        crate::build::build(extractions, options, diagnostics, Some(self))
    }
    pub fn backend(&self, name: Option<&str>, d: &mut Diagnostics) -> Result<String> {
        if let Some(name) = name {
            return self
                .config
                .backend(name)
                .map(|_| name.into())
                .map_err(Error::value);
        }
        self.config.detect_backend(d).map_err(Error::value)?.ok_or_else(||Error::value("No LLM backend configured. Set one of: GEMINI_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY, DEEPSEEK_API_KEY, MOONSHOT_API_KEY, AZURE_OPENAI_API_KEY+AZURE_OPENAI_ENDPOINT, OLLAMA_BASE_URL, or AWS credentials. Pass backend= explicitly to select a provider."))
    }
    pub fn extract_files(
        &self,
        units: &[Unit],
        options: &ExtractOptions,
        d: &mut Diagnostics,
    ) -> Result<Value> {
        let backend = self.backend(options.backend.as_deref(), d)?;
        let (text_units, images) = source::partition(units);
        let user = {
            let mut source = self
                .source
                .lock()
                .map_err(|_| Error::runtime("source cache lock poisoned"))?;
            source::read_files(&text_units, &options.root, &mut source, d)
        };
        let images = source::build_image_refs(
            &images,
            &options.root,
            self.config.vision(&backend) && backend != "claude-cli",
            d,
        );
        let cfg = self.config.backend(&backend).map_err(Error::value)?;
        let default = if !["claude", "claude-cli", "bedrock", "azure"].contains(&backend.as_str()) {
            cfg["max_completion_tokens"]
                .as_i64()
                .filter(|v| *v != 0)
                .or(cfg["max_tokens"].as_i64())
                .unwrap_or(8192)
        } else {
            cfg["max_tokens"].as_i64().unwrap_or(8192)
        };
        let max = crate::llm_config::max_tokens(&self.config.env, default);
        let request = crate::llm_transport::plan(
            &self.config,
            crate::llm_transport::CallOptions {
                backend: &backend,
                model: options.model.as_deref(),
                api_key: options.api_key.as_deref(),
                user: &user,
                images: &images,
                max_tokens: max,
                extract: Some(options.deep_mode),
            },
            d,
        )?;
        let response = self.transport.send(&request)?;
        let mut result = interpret_extraction(&request, &response, d)?;
        let source = self.source.lock();
        match source {
            Ok(mut source) => {
                let n = source::bind_node_evidence(
                    &mut result,
                    &text_units,
                    &options.root,
                    &mut source.source,
                );
                if n > 0 {
                    d.stderr.push(format!("[graphicle] {n} semantic node(s) had no evidence in the source and were flagged verification=unverified"))
                }
            }
            Err(e) => d
                .stderr
                .push(format!("[graphicle] evidence-binding skipped: {e}")),
        }
        Ok(result)
    }
    pub fn call(
        &self,
        prompt: &str,
        backend: &str,
        model: Option<&str>,
        max_tokens: i64,
        usage: &mut Usage,
        d: &mut Diagnostics,
    ) -> Result<String> {
        let request = crate::llm_transport::plan(
            &self.config,
            crate::llm_transport::CallOptions {
                backend,
                model,
                api_key: None,
                user: prompt,
                images: &[],
                max_tokens,
                extract: None,
            },
            d,
        )?;
        let response = self.transport.send(&request)?;
        let (text, reported) = interpret_plain(&request, &response)?;
        usage.add(&reported);
        Ok(text)
    }
    pub fn worker_count(&self, backend: &str, requested: i64, total: usize) -> usize {
        let env = &self.config.env;
        if (backend == "ollama"
            && crate::llm_config::trim(crate::llm_config::get(env, "GRAPHICLE_OLLAMA_PARALLEL"))
                != "1")
            || (backend == "claude-cli"
                && crate::llm_config::trim(crate::llm_config::get(
                    env,
                    "GRAPHICLE_CLAUDE_CLI_PARALLEL",
                )) != "1")
        {
            1
        } else {
            requested.min(total as i64).max(1) as usize
        }
    }
}
fn integer(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(crate::llm_config::parse_integer))
        .unwrap_or(0)
}
fn response_parts<'a>(r: &Request, response: &'a Value) -> Result<(Option<&'a str>, Usage, Value)> {
    let usage = &response["usage"];
    match r.backend.as_str() {
        "claude" => Ok((
            parse::anthropic_text(&response["content"], None),
            Usage {
                input: integer(&usage["input_tokens"]),
                output: integer(&usage["output_tokens"]),
            },
            json!(if response["stop_reason"].as_str() == Some("max_tokens") {
                "length"
            } else {
                "stop"
            }),
        )),
        "bedrock" => Ok((
            Some(parse::bedrock_text(response, "{}")),
            Usage {
                input: integer(&usage["inputTokens"]),
                output: integer(&usage["outputTokens"]),
            },
            json!(if response["stopReason"].as_str() == Some("max_tokens") {
                "length"
            } else {
                "stop"
            }),
        )),
        "claude-cli" => Ok((
            response["result"].as_str(),
            Usage {
                input: [
                    "input_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                ]
                .iter()
                .map(|k| integer(&usage[*k]))
                .sum(),
                output: integer(&usage["output_tokens"]),
            },
            json!(if response["stop_reason"].as_str() == Some("max_tokens") {
                "length"
            } else {
                "stop"
            }),
        )),
        _ => {
            let c = response["choices"]
                .as_array()
                .and_then(|a| a.first())
                .filter(|c| !c["message"].is_null())
                .ok_or_else(|| {
                    Error::value(if r.backend == "azure" {
                        "Azure OpenAI returned empty or filtered response"
                    } else {
                        "LLM returned empty or filtered response"
                    })
                })?;
            Ok((
                c["message"]["content"].as_str(),
                Usage {
                    input: integer(&usage["prompt_tokens"]),
                    output: integer(&usage["completion_tokens"]),
                },
                c["finish_reason"].clone(),
            ))
        }
    }
}
pub fn interpret_extraction(r: &Request, response: &Value, d: &mut Diagnostics) -> Result<Value> {
    let (mut raw, usage, finish) = response_parts(r, response)?;
    let structured = if r.backend == "claude-cli" && response["structured_output"].is_object() {
        Some(
            serde_json::to_string(&response["structured_output"])
                .map_err(|e| Error::value(e.to_string()))?,
        )
    } else {
        None
    };
    if let Some(s) = structured.as_deref() {
        raw = Some(s)
    }
    let mut result = parse::parse(raw.filter(|s| !s.is_empty()).unwrap_or("{}"), d);
    result["input_tokens"] = json!(usage.input);
    result["output_tokens"] = json!(usage.output);
    result["model"] = if r.backend == "claude-cli" {
        json!(
            response["modelUsage"]
                .as_object()
                .and_then(|m| m.keys().next())
                .map(String::as_str)
                .unwrap_or("claude-code-plan")
        )
    } else {
        json!(r.model)
    };
    result["finish_reason"] = finish;
    parse::mark_hollow(&mut result, raw, &r.backend, d);
    if r.backend == "ollama" && usage.output < 50 {
        d.stderr.push("[graphicle] warning: ollama returned very few tokens — likely causes: (1) VRAM pressure: check `nvidia-smi` and reduce chunk size with --token-budget (e.g. --token-budget 4096) or set GRAPHICLE_OLLAMA_NUM_CTX to a smaller value; (2) model too small for JSON instruction following — try a larger model with --model (e.g. --model qwen2.5-coder:14b).".into())
    }
    Ok(result)
}
pub fn interpret_plain(r: &Request, response: &Value) -> Result<(String, Usage)> {
    let (raw, usage, _) = response_parts(r, response)?;
    let text = if r.backend == "bedrock" {
        parse::bedrock_text(response, "")
    } else {
        raw.unwrap_or("")
    };
    Ok((text.into(), usage))
}
impl crate::dedup::TiebreakClient for Runtime {
    fn availability(&self, backend: &str) -> crate::dedup::BackendAvailability {
        use crate::dedup::BackendAvailability::*;
        if self.config.backend(backend).is_err() {
            Unknown
        } else if ["ollama", "bedrock", "claude-cli"].contains(&backend)
            || self.config.api_key(backend).is_ok_and(|s| !s.is_empty())
        {
            Available
        } else {
            MissingCredentials(self.config.format_env_keys(backend).unwrap_or_default())
        }
    }
    fn call(&mut self, prompt: &str, backend: &str, max_tokens: u32) -> crate::Result<String> {
        Ok(Runtime::call(
            self,
            prompt,
            backend,
            None,
            max_tokens as i64,
            &mut Usage::default(),
            &mut Diagnostics::default(),
        )
        .map_err(|e| e.to_string())?)
    }
}
/// Mark every node and edge of a semantic extraction `_origin: "semantic"`
/// (Graphify 0.9.87, #2843). The AST-tier test reads `_origin` when present
/// and otherwise guesses from the shape of `source_location` (`L<line>` reads
/// as AST), so a backend that reports line numbers for a document's sections
/// made those nodes read as AST, and the next incremental rebuild deleted
/// them with the re-extracted code file they seemed to belong to. Extraction
/// stamps its items `ast`; existing stamps are kept.
pub fn stamp_semantic_origin(result: &mut Value) {
    for key in ["nodes", "edges"] {
        for item in result
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(item) = item.as_object_mut() {
                item.entry("_origin").or_insert_with(|| json!("semantic"));
            }
        }
    }
}
pub fn context_exceeded(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "context size",
        "context length",
        "context_length",
        "context window",
        "n_keep",
        "exceeds the available",
        "n_ctx",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "context_length_exceeded",
    ]
    .iter()
    .any(|p| text.contains(p))
}
pub fn resolved_against_root(path: &str, root: &Path) -> std::path::PathBuf {
    let p = Path::new(path);
    let p = if p.is_absolute() {
        p.to_owned()
    } else {
        root.join(p)
    };
    crate::fsutil::resolve_missing(&p).unwrap_or(p)
}
