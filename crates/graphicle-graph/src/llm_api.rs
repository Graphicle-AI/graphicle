// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! JSON bindings and explicitly recorded transport fixtures for qualification.
use crate::{
    Result,
    build::Diagnostics,
    cache::{CacheSession, ExtractionCache},
    file_slice::Unit,
    graph::Graph,
    llm::{self, Error, Runtime, Usage},
    llm_config::{self as cfg, Configuration, Environment},
    llm_labels as labels, llm_parse as parse, llm_source as source,
    llm_transport::{self as transport, Request, Transport},
};
use serde_json::{Map, Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
static SOURCES: LazyLock<Mutex<source::TokenPacker>> =
    LazyLock::new(|| Mutex::new(source::TokenPacker::default()));
fn config(r: &Value, d: &mut Diagnostics) -> Result<Configuration> {
    let mut config = if let Some(env) = r.get("env") {
        Configuration::builtins(
            serde_json::from_value::<Environment>(env.clone()).map_err(|e| e.to_string())?,
        )
    } else {
        Configuration::from_environment(d)
    };
    if let Some(backends) = r["backends"].as_object() {
        config.backends = backends.clone()
    }
    if let Some(backends) = r["backend_overrides"].as_object() {
        for (name, value) in backends {
            config.backends.insert(name.clone(), value.clone());
        }
    }
    if let Some(global) = r["global_providers"].as_str() {
        config.load_custom(
            Path::new(global),
            Path::new(
                r["local_providers"]
                    .as_str()
                    .unwrap_or(".graphicle-graph/providers.json"),
            ),
            d,
        )
    }
    Ok(config)
}
#[derive(Default)]
struct Replay {
    responses: Vec<Value>,
    calls: Mutex<Vec<Value>>,
    sleeps: Mutex<Vec<f64>>,
}
impl Transport for Replay {
    fn send(&self, r: &Request) -> llm::Result<Value> {
        let step = {
            let mut calls = self
                .calls
                .lock()
                .map_err(|_| Error::runtime("replay lock poisoned"))?;
            let i = calls.len();
            calls.push(serde_json::to_value(r).map_err(|e| Error::value(e.to_string()))?);
            self.responses
                .get(i)
                .cloned()
                .ok_or_else(|| Error::runtime("Recorded transport responses exhausted"))?
        };
        if let Some(ms) = step["delay_ms"].as_u64() {
            std::thread::sleep(Duration::from_millis(ms))
        }
        if let Some(error) = step.get("error") {
            return Err(decode_error(error));
        }
        Ok(step.get("response").cloned().unwrap_or(step))
    }
    fn sleep(&self, duration: Duration) {
        self.sleeps.lock().unwrap().push(duration.as_secs_f64());
    }
}
fn decode_error(v: &Value) -> Error {
    if let Some(s) = v.as_str() {
        Error::runtime(s)
    } else {
        serde_json::from_value(v.clone()).unwrap_or_else(|_| Error::runtime(v.to_string()))
    }
}
fn options<T: serde::de::DeserializeOwned>(r: &Value) -> Result<T> {
    serde_json::from_value(r.get("options").cloned().unwrap_or_else(|| r.clone()))
        .map_err(|e| e.to_string())
        .map_err(crate::Error::from)
}
fn units(r: &Value, key: &str) -> Result<Vec<Unit>> {
    serde_json::from_value(r.get(key).cloned().unwrap_or(json!([])))
        .map_err(|e| e.to_string())
        .map_err(crate::Error::from)
}
fn runtime(r: &Value, c: Configuration) -> Result<(Runtime, Option<Arc<Replay>>)> {
    if let Some(responses) = r["responses"].as_array() {
        let replay = Arc::new(Replay {
            responses: responses.clone(),
            ..Default::default()
        });
        Ok((Runtime::with_transport(c, replay.clone()), Some(replay)))
    } else if r["execute"].as_bool() == Some(true) {
        Ok((Runtime::new(c), None))
    } else {
        Err("Model execution requires explicit execute=true or recorded responses".into())
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |key: &str| r[key].as_str().unwrap_or("");
    let text = s("text");
    let function = s("function");
    let mut d = Diagnostics::default();
    let c = config(r, &mut d)?;
    let env = &c.env;
    let root = Path::new(r["root"].as_str().unwrap_or("."));
    let images = || {
        serde_json::from_value::<Vec<source::ImageRef>>(
            r.get("refs")
                .or(r.get("images"))
                .cloned()
                .unwrap_or(json!([])),
        )
        .map_err(|e| e.to_string())
    };
    let cs = || crate::exporters::common::communities(r);
    let mut metadata = Map::new();
    let value = match function {
        "configuration" => {
            json!({"backends":c.backends,"detection_env_vars":c.detection_env_vars()})
        }
        "_load_custom_providers" => {
            let names: std::collections::HashSet<_> = Configuration::builtins(Environment::new())
                .backends
                .keys()
                .cloned()
                .collect();
            json!(
                c.backends
                    .iter()
                    .filter(|(k, _)| !names.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map<_, _>>()
            )
        }
        "_custom_providers_path" => json!(if r["global_"].as_bool().unwrap_or(true) {
            std::env::home_dir()
                .unwrap_or_default()
                .join(".graphicle-graph/providers.json")
        } else {
            PathBuf::from(".graphicle-graph/providers.json")
        }),
        "_resolve_ollama_base_url" => json!(cfg::resolve_ollama_base_url(env, s("default"))),
        "_resolve_max_tokens" => json!(cfg::max_tokens(env, r["default"].as_i64().unwrap_or(8192))),
        "_resolve_max_retries" => json!(cfg::max_retries(env, r["default"].as_i64().unwrap_or(6))),
        "_resolve_max_retry_depth" => json!(cfg::max_retry_depth(
            env,
            r["default"].as_i64().unwrap_or(3)
        )),
        "_resolve_api_timeout" => json!(cfg::timeout(env, r["default"].as_f64().unwrap_or(600.0))),
        "_model_requires_default_temperature" => {
            json!(cfg::requires_default_temperature(s("model")))
        }
        "_resolve_temperature" => json!(cfg::temperature(
            env,
            r.get("default").map(|v| v.as_f64()).unwrap_or(Some(0.0)),
            s("model"),
            &mut d
        )),
        "_bedrock_inference_config" => cfg::bedrock_inference(
            env,
            r["max_tokens"].as_i64().unwrap_or(8192),
            s("model"),
            &mut d,
        ),
        "_thinking_disabled_via_env" => json!(cfg::yes(env, "GRAPHICLE_DISABLE_THINKING")),
        "_no_window_kwargs" => {
            #[cfg(windows)]
            {
                json!({"creationflags":134217728})
            }
            #[cfg(not(windows))]
            {
                json!({})
            }
        }
        "_backend_env_keys" => json!(c.env_keys(s("backend"))?),
        "_format_backend_env_keys" => json!(c.format_env_keys(s("backend"))?),
        "_default_model_for_backend" => json!(c.model(s("backend"))?),
        "_backend_supports_vision" => json!(c.vision(s("backend"))),
        "backend_detection_env_vars" => json!(c.detection_env_vars()),
        "detect_backend" => json!(c.detect_backend(&mut d)?),
        "detect_backend_with_source" => json!(c.detect_backend_with_source(&mut d)?),
        "backend_disabled_by_env" => json!(c.backend_disabled_by_env()),
        "_claude_cli_available" => json!(cfg::claude_command(env).is_some()),
        "provider_base_url_ok" => json!(cfg::provider_base_url_ok(
            s("base_url"),
            s("name"),
            r["warn"].as_bool().unwrap_or(true),
            &mut d
        )),
        "_ollama_host_is_link_local_or_metadata" => {
            let ips: Vec<std::net::IpAddr> = r["addresses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|s| s.parse().ok())
                .collect();
            json!(if r.get("addresses").is_some() {
                cfg::metadata_host_with(s("host"), &ips)
            } else {
                cfg::metadata_host(s("host"))
            })
        }
        "_validate_ollama_base_url" => {
            let addresses: Vec<std::net::IpAddr> = r["addresses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|s| s.parse().ok())
                .collect();
            cfg::validate_ollama_url_with(
                s("url"),
                r["warn"].as_bool().unwrap_or(true),
                |h| {
                    if r.get("addresses").is_some() {
                        cfg::metadata_host_with(h, &addresses)
                    } else {
                        cfg::metadata_host(h)
                    }
                },
                &mut d,
            )?;
            Value::Null
        }
        "estimate_cost" => json!(c.estimate_cost(
            s("backend"),
            r["input_tokens"].as_f64().unwrap_or(0.0),
            r["output_tokens"].as_f64().unwrap_or(0.0)
        )),
        "_extraction_system" => json!(llm::extraction_system(r["deep"].as_bool().unwrap_or(false))),
        "_file_to_text" => json!(source::file_to_text(
            &mut SOURCES.lock().unwrap().source,
            Path::new(s("path")),
            Some(&mut d)
        )?),
        "_resolve_ollama_seed" => json!(cfg::resolve_ollama_seed(env)?),
        "_stamp_semantic_origin" => {
            let mut result = r["result"].clone();
            llm::stamp_semantic_origin(&mut result);
            result
        }
        "_resolve_under_root" => json!(source::resolve_under_root(Path::new(s("path")), root)),
        "_neutralise_injection_sentinels" => json!(source::neutralise_sentinels(text)),
        "_wrap_untrusted" => json!(source::wrap_untrusted(s("rel"), s("content"))),
        "_read_files" => json!(source::read_files(
            &units(r, "units")?,
            root,
            &mut SOURCES.lock().unwrap(),
            &mut d
        )),
        "_label_identifiers" => json!(source::label_identifiers(s("label"))),
        "_dispatched_source_text" => {
            let source = source::dispatched_source(
                &units(r, "units")?,
                root,
                &mut SOURCES.lock().unwrap().source,
            );
            json!(source)
        }
        "_bind_node_evidence" => {
            let mut v = r["result"].clone();
            let count = source::bind_node_evidence(
                &mut v,
                &units(r, "text_units")?,
                root,
                &mut SOURCES.lock().unwrap().source,
            );
            json!({"result":v,"count":count})
        }
        "_is_vision_image" => json!(source::is_vision_image(Path::new(s("path")))),
        "_partition_semantic_files" => {
            let (t, i) = source::partition(&units(r, "units")?);
            json!([t, i])
        }
        "_build_image_refs" => json!(source::build_image_refs(
            &serde_json::from_value::<Vec<PathBuf>>(r["image_files"].clone())
                .map_err(|e| e.to_string())?,
            root,
            r["read_bytes"].as_bool().unwrap_or(true),
            &mut d
        )),
        "_strip_pixels" => json!(source::strip_pixels(&images()?)),
        "image_ref_properties" => {
            let refs = images()?;
            json!(
                refs.iter()
                    .map(|r| json!({"b64":r.b64(),"bedrock_format":r.bedrock_format()}))
                    .collect::<Vec<_>>()
            )
        }
        "_image_notes" => json!(source::image_notes(
            &images()?,
            r["with_paths"].as_bool().unwrap_or(false)
        )),
        "_with_image_notes" => json!(source::with_image_notes(
            s("user_message"),
            &images()?,
            r["with_paths"].as_bool().unwrap_or(false)
        )),
        "_anthropic_content" => source::anthropic_content(s("user_message"), &images()?),
        "_openai_content" => source::openai_content(s("user_message"), &images()?),
        "_bedrock_content" => source::bedrock_content(s("user_message"), &images()?),
        "_sanitize_fragment" => parse::sanitize(r["parsed"].clone(), &mut d),
        "_balanced_object" => json!(parse::balanced_object(
            text,
            r["start"].as_u64().unwrap_or(0) as usize
        )),
        "_json_object_candidates" => json!(parse::object_candidates(text)),
        "_json_fragment_candidates" => json!(parse::fragment_candidates(text)),
        "_parse_llm_json" => parse::parse(s("raw"), &mut d),
        "_anthropic_response_text" => {
            json!(parse::anthropic_text(&r["content"], r["default"].as_str()))
        }
        "_bedrock_response_text" => json!(parse::bedrock_text(&r["resp"], s("default"))),
        "_response_is_hollow" => json!(parse::hollow(r["raw_content"].as_str(), &r["parsed"])),
        "_mark_hollow" => {
            let mut v = r["result"].clone();
            parse::mark_hollow(&mut v, r["raw_content"].as_str(), s("backend"), &mut d);
            v
        }
        "_envelope_after_preamble" => {
            parse::envelope_after_preamble(s("stdout")).unwrap_or(Value::Null)
        }
        "_claude_cli_envelope" => parse::cli_envelope(s("stdout"))?,
        "_claude_cli_error" => json!(parse::cli_error(s("stdout"))),
        "_pdf_text_for_estimate" => json!(SOURCES.lock().unwrap().pdf_text(Path::new(s("path")))),
        "_estimate_file_tokens" => json!(SOURCES.lock().unwrap().estimate(
            &serde_json::from_value::<Unit>(r["unit"].clone()).map_err(|e| e.to_string())?,
            r["use_tokenizer"].as_bool().unwrap_or(true)
        )),
        "_pack_chunks_by_tokens" => json!(SOURCES.lock().unwrap().pack(
            &units(r, "files")?,
            r["token_budget"].as_i64().unwrap_or(60000),
            r["use_tokenizer"].as_bool().unwrap_or(true)
        )?),
        "_looks_like_context_exceeded" => json!(llm::context_exceeded(s("error"))),
        "_looks_like_timeout" => json!(decode_error(&r["error"]).kind == llm::ErrorKind::Timeout),
        "_mark_partial" => {
            let mut v = r["result"].clone();
            parse::mark_partial(&mut v);
            v
        }
        "_chunk_partial_files" => json!(parse::chunk_partial_files(&units(r, "chunk")?)),
        "_merged_partial_files" => json!(parse::merged_partial_files(
            &r["results"]
                .as_array()
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
        )),
        "_partial_source_files" => json!(parse::partial_source_files(&r["result"])),
        "_strip_partial_markers" => {
            let mut v = r["result"].clone();
            parse::strip_partial_markers(&mut v);
            v
        }
        "_merge_into" => {
            let mut v = r["merged"].clone();
            parse::merge_into(&mut v, &r["result"]);
            v
        }
        "_placeholder_community_labels" => json!(labels::placeholders(&cs()?)),
        "_community_label_lines" => {
            let g = Graph::from_node_link(&r["G"], "edges")?;
            let (lines, cids) = labels::lines(
                &g,
                &cs()?,
                r["gods"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                r["max_communities"].as_i64(),
                r["top_k"].as_i64().unwrap_or(12),
            );
            json!([lines, cids])
        }
        "_parse_label_response" => json!(
            labels::parse_labels(
                text,
                &serde_json::from_value::<Vec<i64>>(r["labeled_cids"].clone())
                    .map_err(|e| e.to_string())?
            )
            .map_err(|e| e.to_string())?
        ),
        "request_plan" | "interpret_extraction" | "interpret_plain" => {
            let request = transport::plan(
                &c,
                crate::llm_transport::CallOptions {
                    backend: s("backend"),
                    model: r["model"].as_str(),
                    api_key: r["api_key"].as_str(),
                    user: s("user_message"),
                    images: &images()?,
                    max_tokens: r["max_tokens"].as_i64().unwrap_or(8192),
                    extract: if r["plain"].as_bool() == Some(true) {
                        None
                    } else {
                        Some(r["deep_mode"].as_bool().unwrap_or(false))
                    },
                },
                &mut d,
            )
            .map_err(|e| e.to_string())?;
            match function {
                "interpret_extraction" => {
                    llm::interpret_extraction(&request, &r["response"], &mut d)
                        .map_err(|e| e.to_string())?
                }
                "interpret_plain" => {
                    let (text, usage) = llm::interpret_plain(&request, &r["response"])
                        .map_err(|e| e.to_string())?;
                    json!({"text":text,"usage":usage})
                }
                _ => json!(request),
            }
        }
        "_call_llm"
        | "extract_files_direct"
        | "adaptive"
        | "extract_corpus_parallel"
        | "label_communities"
        | "generate_community_labels"
        | "_label_batch_with_retry"
        | "send_plan" => {
            let (rt, replay) = runtime(r, c.clone())?;
            let mut usage = Usage::default();
            let mut events = vec![];
            let result: llm::Result<Value> = match function {
                "_call_llm" => rt
                    .call(
                        s("prompt"),
                        s("backend"),
                        r["model"].as_str(),
                        r["max_tokens"].as_i64().unwrap_or(200),
                        &mut usage,
                        &mut d,
                    )
                    .map(|s| json!(s)),
                "send_plan" => {
                    let request = transport::plan(
                        &c,
                        crate::llm_transport::CallOptions {
                            backend: s("backend"),
                            model: r["model"].as_str(),
                            api_key: r["api_key"].as_str(),
                            user: s("user_message"),
                            images: &images()?,
                            max_tokens: r["max_tokens"].as_i64().unwrap_or(8192),
                            extract: if r["plain"].as_bool() == Some(true) {
                                None
                            } else {
                                Some(r["deep_mode"].as_bool().unwrap_or(false))
                            },
                        },
                        &mut d,
                    )
                    .map_err(|e| e.to_string())?;
                    // As every model call does: Ollama's native /api/chat
                    // first, falling back to /v1 (0.9.91, #4413).
                    transport::send(&*rt.transport, &request, &mut d)
                }
                "extract_files_direct" => {
                    rt.extract_files(&units(r, "files")?, &options(r)?, &mut d)
                }
                "adaptive" => rt.adaptive(
                    &units(r, "chunk")?,
                    &options(r)?,
                    r["max_depth"].as_i64().unwrap_or(3),
                    r["depth"].as_i64().unwrap_or(0),
                    &mut d,
                ),
                "extract_corpus_parallel" => {
                    let mut cache = CacheSession(ExtractionCache::default());
                    rt.extract_corpus(
                        &units(r, "files")?,
                        &options(r)?,
                        &mut cache.0,
                        &mut d,
                        &mut |e| events.push(json!(e)),
                    )
                }
                "_label_batch_with_retry" => rt
                    .label_batch(
                        &serde_json::from_value::<Vec<i64>>(r["batch_cids"].clone())
                            .map_err(|e| e.to_string())?,
                        &serde_json::from_value::<Vec<String>>(r["batch_lines"].clone())
                            .map_err(|e| e.to_string())?,
                        labels::BatchOptions {
                            backend: s("backend"),
                            model: r["model"].as_str(),
                            depth: r["depth"].as_i64().unwrap_or(0),
                            max_depth: r["max_depth"].as_i64().unwrap_or(3),
                        },
                        &mut usage,
                        &mut d,
                    )
                    .map(|v| json!(v)),
                _ => {
                    let g = Graph::from_node_link(&r["G"], "edges")?;
                    let cs = cs()?;
                    let options = options(r)?;
                    if function == "generate_community_labels" {
                        let (labels, source) =
                            rt.generate_labels(&g, &cs, &options, &mut usage, &mut d);
                        Ok(json!([labels, source]))
                    } else {
                        rt.label_communities(&g, &cs, &options, &mut usage, &mut d)
                            .map(|v| json!(v))
                    }
                }
            };
            metadata.insert("usage".into(), json!(usage));
            metadata.insert("events".into(), json!(events));
            if let Some(replay) = replay {
                metadata.insert("calls".into(), json!(*replay.calls.lock().unwrap()));
                metadata.insert("sleeps".into(), json!(*replay.sleeps.lock().unwrap()));
            }
            match result {
                Ok(v) => v,
                Err(e) => {
                    metadata.insert("error".into(), json!(e));
                    Value::Null
                }
            }
        }
        "adaptive_results" | "corpus_results" => {
            // The oracle injects extraction results here to isolate retry and
            // checkpoint policy from provider transport, not to emulate it.
            let replay = Arc::new(Replay::default());
            let rt = Runtime::with_transport(c.clone(), replay.clone());
            let calls = Mutex::new(Vec::<Value>::new());
            let responses = r["results"].as_array().cloned().unwrap_or_default();
            let by_input = function == "corpus_results"
                && responses.iter().all(|step| step.get("files").is_some());
            let responses = Mutex::new(responses);
            let callback = |chunk: &[Unit], _: &llm::ExtractOptions, _: &mut Diagnostics| {
                let input = json!(chunk);
                calls
                    .lock()
                    .map_err(|_| Error::runtime("Replay call lock poisoned"))?
                    .push(input.clone());
                let step = {
                    let mut pending = responses
                        .lock()
                        .map_err(|_| Error::runtime("Replay result lock poisoned"))?;
                    let index = if by_input {
                        pending
                            .iter()
                            .position(|step| step.get("files") == Some(&input))
                    } else {
                        (!pending.is_empty()).then_some(0)
                    }
                    .ok_or_else(|| {
                        Error::runtime("Recorded extraction input has no remaining result")
                    })?;
                    pending.remove(index)
                };
                if let Some(expected) = step.get("files")
                    && *expected != input
                {
                    return Err(Error::runtime("Recorded extraction input mismatch"));
                }
                if let Some(ms) = step["delay_ms"].as_u64() {
                    std::thread::sleep(Duration::from_millis(ms))
                }
                if let Some(error) = step.get("error") {
                    return Err(decode_error(error));
                }
                Ok(step.get("result").cloned().unwrap_or(step))
            };
            let mut events = vec![];
            let result = if function == "adaptive_results" {
                rt.adaptive_using(
                    &units(r, "chunk")?,
                    &options(r)?,
                    r["max_depth"].as_i64().unwrap_or(3),
                    r["depth"].as_i64().unwrap_or(0),
                    &mut d,
                    &callback,
                )
            } else {
                let mut cache = CacheSession(ExtractionCache::default());
                rt.corpus_using(
                    &units(r, "files")?,
                    &options(r)?,
                    &mut cache.0,
                    &mut d,
                    &mut |e| events.push(json!(e)),
                    &callback,
                )
            };
            metadata.insert("calls".into(), json!(*calls.lock().unwrap()));
            metadata.insert("sleeps".into(), json!(*replay.sleeps.lock().unwrap()));
            metadata.insert("events".into(), json!(events));
            match result {
                Ok(v) => v,
                Err(e) => {
                    metadata.insert("error".into(), json!(e));
                    Value::Null
                }
            }
        }
        "reconcile" => {
            let mut v = r["merged"].clone();
            let chunks = serde_json::from_value::<Vec<Vec<Unit>>>(r["chunks"].clone())
                .map_err(|e| e.to_string())?;
            crate::llm_extract::reconcile(&mut v, &chunks, root, &mut d);
            v
        }
        _ => return Err(format!("Unknown llm function {function}").into()),
    };
    metadata.insert("value".into(), value);
    metadata.insert("stdout".into(), json!(d.stdout));
    metadata.insert("stderr".into(), json!(d.stderr));
    Ok(Value::Object(metadata))
}
