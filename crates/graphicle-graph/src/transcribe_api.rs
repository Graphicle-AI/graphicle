// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/transcribe.py
//! Differential-check boundary. Application callers use transcribe::Runtime.
use crate::{
    Result,
    build::Diagnostics,
    transcribe::{self, SpeechEngine, Transcript},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

struct ReplaySpeech {
    responses: Mutex<Vec<Value>>,
    calls: Mutex<Vec<Value>>,
}
impl SpeechEngine for ReplaySpeech {
    fn transcribe(&self, path: &Path, model: &str, prompt: &str) -> Result<Transcript> {
        self.push_call(path, model, prompt);
        let mut responses = self.responses.lock().unwrap();
        let index = responses
            .iter()
            .position(|r| r.get("path").is_none_or(|p| p == &json!(path)))
            .ok_or("No recorded speech response")?;
        let response = responses.remove(index);
        if let Some(e) = response["error"].as_str() {
            return Err(e.into());
        }
        let segments = response["segments"]
            .as_array()
            .ok_or("Expected recorded speech segments")?
            .iter()
            .map(|s| {
                s.as_str()
                    .map(String::from)
                    .ok_or_else(|| "Expected segment text".into())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Transcript {
            segments,
            language: response["language"].as_str().unwrap_or("unknown").into(),
        })
    }
}
impl ReplaySpeech {
    fn push_call(&self, path: &Path, model: &str, prompt: &str) {
        self.calls
            .lock()
            .unwrap()
            .push(json!({"path":path,"model":model,"initial_prompt":prompt,"beam_size":5}));
    }
}
struct RequireExecution;
impl SpeechEngine for RequireExecution {
    fn transcribe(&self, _: &Path, _: &str, _: &str) -> Result<Transcript> {
        Err(
            "Native speech execution requires execute=true or recorded qualification responses"
                .into(),
        )
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let env = r["env"]
        .as_object()
        .map(|e| {
            e.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_else(|| std::env::vars().collect());
    let mut runtime = transcribe::Runtime::from_env(env);
    let replay = r["responses"].as_array().map(|responses| {
        Arc::new(ReplaySpeech {
            responses: Mutex::new(responses.clone()),
            calls: Mutex::new(vec![]),
        })
    });
    if let Some(replay) = &replay {
        runtime.speech = replay.clone();
    } else if r["execute"] != true {
        runtime.speech = Arc::new(RequireExecution);
    }
    let mut d = Diagnostics::default();
    let path = || r["path"].as_str().ok_or("Expected path");
    let out = r["output_dir"].as_str().map(Path::new);
    let prompt = r["initial_prompt"].as_str();
    let value = match r["function"].as_str().unwrap_or("transcribe") {
        "constants" => {
            json!({"VIDEO_EXTENSIONS":transcribe::VIDEO_EXTENSIONS,"URL_PREFIXES":transcribe::URL_PREFIXES,"DEFAULT_MODEL":transcribe::DEFAULT_MODEL,"FALLBACK_PROMPT":transcribe::FALLBACK_PROMPT})
        }
        "is_url" => json!(transcribe::is_url(path()?)),
        "_model_name" => json!(transcribe::model_name(&runtime.env)),
        "build_whisper_prompt" => json!(transcribe::build_whisper_prompt(
            r["nodes"].as_array().ok_or("Expected nodes")?,
            &runtime.env
        )?),
        "audio_stem" => json!(crate::transcribe_native::audio_stem(
            r["url"].as_str().ok_or("Expected URL")?
        )),
        "cached_audio" => json!(crate::transcribe_native::cached_audio(
            r["url"].as_str().ok_or("Expected URL")?,
            out.ok_or("Expected output directory")?
        )),
        "download_audio" => {
            if r["execute"] != true {
                return Err("Audio download execution requires execute=true".into());
            }
            json!(runtime.downloader.download(
                r["url"].as_str().ok_or("Expected URL")?,
                out.ok_or("Expected output directory")?,
                &mut d
            )?)
        }
        "model_path" => {
            if r["execute"] != true {
                return Err("Model resolution requires execute=true".into());
            }
            json!(crate::transcribe_native::model_path(
                r["model"].as_str().ok_or("Expected model")?,
                &runtime.env
            )?)
        }
        "transcribe" => {
            json!(runtime.transcribe(path()?, out, prompt, r["force"] == true, &mut d)?)
        }
        "transcribe_all" => {
            let inputs = r["paths"]
                .as_array()
                .ok_or("Expected paths")?
                .iter()
                .map(|p| {
                    p.as_str()
                        .map(String::from)
                        .ok_or_else(|| "Expected path".into())
                })
                .collect::<Result<Vec<_>>>()?;
            json!(runtime.transcribe_all(&inputs, out, prompt, &mut d))
        }
        method => return Err(format!("Unknown transcription function {method}").into()),
    };
    Ok(
        json!({"value":value,"stdout":d.stdout,"stderr":d.stderr,"speech_calls":replay.as_ref().map(|s| s.calls.lock().unwrap().clone()).unwrap_or_default()}),
    )
}
