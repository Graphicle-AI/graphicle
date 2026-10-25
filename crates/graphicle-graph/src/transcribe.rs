// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/transcribe.py
//! Port of graphify/transcribe.py. Native orchestration, Whisper and media IO.
use crate::{Result, build::Diagnostics, dedup_strings::whitespace, paths};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use crate::transcribe_native::{NativeDownloader, NativeSpeech};
pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mp4", ".mov", ".webm", ".mkv", ".avi", ".m4v", ".mp3", ".wav", ".m4a", ".ogg",
];
pub const URL_PREFIXES: &[&str] = &["http://", "https://", "www."];
pub const DEFAULT_MODEL: &str = "base";
pub const FALLBACK_PROMPT: &str = "Use proper punctuation and paragraph breaks.";
pub type Environment = BTreeMap<String, String>;

pub fn model_name(env: &Environment) -> &str {
    env.get("GRAPHICLE_WHISPER_MODEL")
        .map(String::as_str)
        .unwrap_or(DEFAULT_MODEL)
}
pub fn is_url(path: &str) -> bool {
    URL_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}
pub fn build_whisper_prompt(nodes: &[Value], env: &Environment) -> Result<String> {
    if nodes.is_empty() {
        return Ok(FALLBACK_PROMPT.into());
    }
    if let Some(override_) = env
        .get("GRAPHICLE_WHISPER_PROMPT")
        .filter(|s| !s.is_empty())
    {
        return Ok(override_.clone());
    }
    let labels: Vec<_> = nodes
        .iter()
        .take(10)
        .filter_map(|n| crate::build::truthy(&n["label"]).then_some(&n["label"]))
        .take(5)
        .map(|v| v.as_str().ok_or("TypeError: topic labels must be strings"))
        .collect::<std::result::Result<_, _>>()?;
    if labels.is_empty() {
        return Ok(FALLBACK_PROMPT.into());
    }
    Ok(format!(
        "Technical discussion about {}. {FALLBACK_PROMPT}",
        labels.join(", ")
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transcript {
    pub segments: Vec<String>,
    pub language: String,
}
pub trait SpeechEngine: Send + Sync {
    fn transcribe(&self, path: &Path, model: &str, initial_prompt: &str) -> Result<Transcript>;
}
pub trait Downloader: Send + Sync {
    fn download(&self, url: &str, output_dir: &Path, d: &mut Diagnostics) -> Result<PathBuf>;
}
pub struct Runtime {
    pub env: Environment,
    pub speech: Arc<dyn SpeechEngine>,
    pub downloader: Arc<dyn Downloader>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self::from_env(std::env::vars().collect())
    }
}
impl Runtime {
    pub fn from_env(env: Environment) -> Self {
        Self {
            speech: Arc::new(NativeSpeech { env: env.clone() }),
            downloader: Arc::new(NativeDownloader { env: env.clone() }),
            env,
        }
    }
    pub fn transcribe(
        &self,
        input: &str,
        output_dir: Option<&Path>,
        initial_prompt: Option<&str>,
        force: bool,
        d: &mut Diagnostics,
    ) -> Result<PathBuf> {
        let default_dir = paths::out_path(&["transcripts".into()]);
        let out = output_dir
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(&default_dir);
        std::fs::create_dir_all(out).map_err(|e| format!("OSError: {e}"))?;
        let audio = if is_url(input) {
            self.downloader.download(input, &out.join("downloads"), d)?
        } else {
            PathBuf::from(input)
        };
        let stem = audio.file_stem().unwrap_or_default().to_string_lossy();
        let transcript = out.join(format!("{stem}.txt"));
        if transcript.exists() && !force {
            return Ok(transcript);
        }
        let model = model_name(&self.env);
        let prompt = initial_prompt
            .filter(|s| !s.is_empty())
            .unwrap_or(FALLBACK_PROMPT);
        d.stdout.push(format!(
            "  transcribing {} (model={model}) ...",
            audio.file_name().unwrap_or_default().to_string_lossy()
        ));
        let result = self.speech.transcribe(&audio, model, prompt)?;
        let lines: Vec<_> = result
            .segments
            .iter()
            .map(|s| s.trim_matches(whitespace))
            .filter(|s| !s.is_empty())
            .collect();
        std::fs::write(&transcript, lines.join("\n")).map_err(|e| format!("OSError: {e}"))?;
        d.stdout.push(format!(
            "  transcript saved -> {} (lang={}, {} segments)",
            transcript.display(),
            result.language,
            lines.len()
        ));
        Ok(transcript)
    }
    pub fn transcribe_all(
        &self,
        inputs: &[String],
        output_dir: Option<&Path>,
        initial_prompt: Option<&str>,
        d: &mut Diagnostics,
    ) -> Vec<String> {
        inputs
            .iter()
            .filter_map(|input| {
                match self.transcribe(input, output_dir, initial_prompt, false, d) {
                    Ok(path) => Some(path.to_string_lossy().into_owned()),
                    Err(e) => {
                        let e = e.to_string();
                        let message = e
                            .split_once(": ")
                            .filter(|(kind, _)| kind.ends_with("Error"))
                            .map_or(e.as_str(), |(_, m)| m);
                        d.stdout.push(format!(
                            "  warning: could not transcribe {input}: {message}"
                        ));
                        None
                    }
                }
            })
            .collect()
    }
}
