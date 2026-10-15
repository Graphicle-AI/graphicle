// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Bedrock Converse through the native AWS SDK and its credential chain.
use crate::{
    llm::{Error, Result},
    llm_config::{get, trim},
    llm_transport::Request,
};
use aws_sdk_bedrockruntime::{
    config::{BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig},
    primitives::Blob,
    types::{
        ContentBlock, ConversationRole, ConverseOutput, ImageBlock, ImageFormat, ImageSource,
        InferenceConfiguration, Message, SystemContentBlock,
    },
};
use serde_json::{Value, json};
use std::time::Duration;

pub fn send(r: &Request) -> Result<Value> {
    // Each blocking worker owns its runtime; this also works when the public
    // API is called by a host that already has a Tokio runtime.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| Error::runtime(e.to_string()))?
                    .block_on(converse(r))
            })
            .join()
            .map_err(|_| Error::runtime("Bedrock worker panicked"))?
    })
}
async fn converse(r: &Request) -> Result<Value> {
    let region = ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .iter()
        .map(|k| get(&r.env, k))
        .find(|s| !s.is_empty())
        .unwrap_or("us-east-1");
    let retry = RetryConfig::adaptive().with_max_attempts(
        u32::try_from(r.retries.saturating_add(1))
            .map_err(|_| Error::value("Bedrock retry budget exceeds u32"))?,
    );
    let timeouts = TimeoutConfig::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(
            Duration::try_from_secs_f64(r.timeout)
                .map_err(|_| Error::value("Invalid Bedrock read timeout"))?,
        )
        .build();
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.to_owned()))
        .retry_config(retry)
        .timeout_config(timeouts);
    if !get(&r.env, "AWS_PROFILE").is_empty() {
        loader = loader.profile_name(get(&r.env, "AWS_PROFILE"))
    }
    if !get(&r.env, "AWS_ACCESS_KEY_ID").is_empty()
        && !get(&r.env, "AWS_SECRET_ACCESS_KEY").is_empty()
    {
        loader = loader.credentials_provider(aws_sdk_bedrockruntime::config::Credentials::new(
            get(&r.env, "AWS_ACCESS_KEY_ID"),
            get(&r.env, "AWS_SECRET_ACCESS_KEY"),
            r.env.get("AWS_SESSION_TOKEN").cloned(),
            None,
            "graphicle-environment",
        ));
    }
    let shared = loader.load().await;
    let mut config = aws_sdk_bedrockruntime::config::Builder::from(&shared);
    if let Some(endpoint) = ["AWS_ENDPOINT_URL_BEDROCK_RUNTIME", "AWS_ENDPOINT_URL"]
        .iter()
        .map(|k| trim(get(&r.env, k)))
        .find(|s| !s.is_empty())
    {
        config = config.endpoint_url(endpoint)
    }
    let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
    let b = &r.body;
    let mut call = client.converse().model_id(&r.model);
    if let Some(a) = b["system"].as_array() {
        for s in a {
            if let Some(text) = s["text"].as_str() {
                call = call.system(SystemContentBlock::Text(text.into()))
            }
        }
    }
    for m in b["messages"]
        .as_array()
        .ok_or_else(|| Error::value("Missing Bedrock messages"))?
    {
        let mut message =
            Message::builder().role(ConversationRole::from(m["role"].as_str().unwrap_or("user")));
        for block in m["content"]
            .as_array()
            .ok_or_else(|| Error::value("Missing Bedrock content"))?
        {
            if let Some(text) = block["text"].as_str() {
                message = message.content(ContentBlock::Text(text.into()))
            } else if block["image"].is_object() {
                let image = &block["image"];
                let raw = image["source"]["bytes"]
                    .as_array()
                    .ok_or_else(|| Error::value("Bedrock image bytes missing"))?
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|v| u8::try_from(v).ok())
                            .ok_or_else(|| Error::value("Invalid Bedrock image byte"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let image = ImageBlock::builder()
                    .format(ImageFormat::from(image["format"].as_str().unwrap_or("png")))
                    .source(ImageSource::Bytes(Blob::new(raw)))
                    .build()
                    .map_err(|e| Error::value(e.to_string()))?;
                message = message.content(ContentBlock::Image(image));
            }
        }
        call = call.messages(message.build().map_err(|e| Error::value(e.to_string()))?);
    }
    let mut inference = InferenceConfiguration::builder().max_tokens(
        i32::try_from(
            b["inferenceConfig"]["maxTokens"]
                .as_i64()
                .ok_or_else(|| Error::value("Missing Bedrock maxTokens"))?,
        )
        .map_err(|_| Error::value("Bedrock maxTokens exceeds i32"))?,
    );
    if let Some(t) = b["inferenceConfig"]["temperature"].as_f64() {
        inference = inference.temperature(t as f32)
    }
    let response = call
        .inference_config(inference.build())
        .send()
        .await
        .map_err(|e| {
            use aws_sdk_bedrockruntime::error::ProvideErrorMetadata;
            if let Some(service) = e.as_service_error() {
                Error::runtime(format!(
                    "Bedrock API error ({}): {}",
                    service.code().unwrap_or("Unknown"),
                    service.message().unwrap_or("")
                ))
            } else if matches!(&e, aws_sdk_bedrockruntime::error::SdkError::TimeoutError(_))
                || matches!(&e, aws_sdk_bedrockruntime::error::SdkError::DispatchFailure(f) if f.is_timeout()) {
                Error::timeout(e.to_string())
            } else {
                Error::transport(e.to_string())
            }
        })?;
    let content = match response.output() {
        Some(ConverseOutput::Message(m)) => m
            .content()
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text(s) = b {
                    Some(json!({"text":s}))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
        _ => vec![],
    };
    let usage = response.usage();
    Ok(
        json!({"output":{"message":{"content":content}},"usage":{"inputTokens":usage.map(|u|u.input_tokens()).unwrap_or(0),"outputTokens":usage.map(|u|u.output_tokens()).unwrap_or(0)},"stopReason":response.stop_reason().as_str()}),
    )
}
