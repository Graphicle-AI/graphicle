// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/transcribe.py
//! Native inference and the separately installed yt-dlp media downloader.
use crate::{
    Result,
    build::Diagnostics,
    llm_config, process, security,
    transcribe::{Downloader, Environment, SpeechEngine, Transcript},
};
use security::Transport;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::Duration,
};

pub static MODELS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/transcribe/models.json"))
        .expect("checked model catalogue")
});
pub fn audio_stem(url: &str) -> String {
    format!("yt_{:.12}", format!("{:x}", Sha1::digest(url.as_bytes())))
}
pub fn cached_audio(url: &str, out: &Path) -> Option<PathBuf> {
    let stem = audio_stem(url);
    [".m4a", ".opus", ".mp3", ".ogg", ".wav", ".webm"]
        .iter()
        .map(|ext| out.join(format!("{stem}{ext}")))
        .find(|p| p.exists())
}
pub struct NativeDownloader {
    pub env: Environment,
}
impl Downloader for NativeDownloader {
    fn download(&self, url: &str, out: &Path, d: &mut Diagnostics) -> Result<PathBuf> {
        // Preserve validation before both dependency discovery and cached reuse.
        security::validate_url(url)?;
        let configured = self
            .env
            .get("GRAPHICLE_YT_DLP")
            .map(String::as_str)
            .unwrap_or("yt-dlp");
        let executable = llm_config::executable(configured, &self.env).ok_or(
            "ImportError: URL audio download requires the yt-dlp executable. Install yt-dlp or set GRAPHICLE_YT_DLP."
        )?;
        std::fs::create_dir_all(out).map_err(|e| format!("OSError: {e}"))?;
        if let Some(path) = cached_audio(url, out) {
            d.stdout.push(format!(
                "  cached audio: {}",
                path.file_name().unwrap().to_string_lossy()
            ));
            return Ok(path);
        }
        let stem = audio_stem(url);
        let template = out.join(format!("{stem}.%(ext)s"));
        d.stdout.push(format!(
            "  downloading audio: {} ...",
            url.chars().take(80).collect::<String>()
        ));
        let result = process::capture(
            Command::new(executable)
                .args([
                    "--ignore-config",
                    "--quiet",
                    "--no-warnings",
                    "--no-progress",
                    "--no-playlist",
                    "--format",
                    "bestaudio[ext=m4a]/bestaudio/best",
                    "--output",
                ])
                .arg(&template)
                .args(["--print-json", "--no-simulate", "--", url]),
            Duration::from_secs(900),
        )
        .map_err(|e| format!("OSError: yt-dlp: {e}"))?;
        if !result.success {
            return Err(format!(
                "RuntimeError: yt-dlp failed: {}",
                String::from_utf8_lossy(&result.stderr)
                    .trim()
                    .chars()
                    .take(2000)
                    .collect::<String>()
            )
            .into());
        }
        let info: Value = serde_json::from_slice(&result.stdout)
            .map_err(|e| format!("RuntimeError: invalid yt-dlp metadata: {e}"))?;
        let ext = info["ext"].as_str().unwrap_or("m4a");
        if ext.is_empty() || !ext.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err("RuntimeError: invalid audio extension in yt-dlp metadata".into());
        }
        let path = out.join(format!("{stem}.{ext}"));
        if path.is_file() {
            return Ok(path);
        }
        for entry in std::fs::read_dir(out).map_err(|e| format!("OSError: {e}"))? {
            let path = entry.map_err(|e| format!("OSError: {e}"))?.path();
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{stem}.")))
                && path.is_file()
            {
                return Ok(path);
            }
        }
        Err("RuntimeError: yt-dlp reported success without a downloaded audio file".into())
    }
}
pub fn model_cache(env: &Environment) -> PathBuf {
    if let Some(path) = env.get("GRAPHICLE_WHISPER_CACHE") {
        return PathBuf::from(path);
    }
    let base = env
        .get("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env.get("HOME")
                .or_else(|| env.get("USERPROFILE"))
                .map_or_else(std::env::temp_dir, |p| Path::new(p).join(".cache"))
        });
    base.join("graphicle").join("whisper")
}
fn catalogue_name(name: &str) -> &str {
    match name {
        "large" => "large-v3",
        "turbo" => "large-v3-turbo",
        _ => name,
    }
}
pub fn model_path(name: &str, env: &Environment) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let name = catalogue_name(name);
    let entry = &MODELS["models"][name];
    if entry.is_null() {
        return Err(format!(
            "ValueError: unknown native Whisper model {name:?}; use a model name or a local GGML .bin file"
        ).into());
    }
    let directory = model_cache(env);
    let path = directory.join(format!("ggml-{name}.bin"));
    let expected_bytes = entry["bytes"].as_u64().unwrap();
    let expected_hash = entry["sha256"].as_str().unwrap();
    if path.is_file() {
        verify_model(&path, expected_bytes, expected_hash)?;
        return Ok(path);
    }
    if llm_config::yes(env, "GRAPHICLE_WHISPER_OFFLINE") {
        return Err(format!(
            "FileNotFoundError: native Whisper model is not cached at {}; offline mode prevents downloading",
            path.display()
        ).into());
    }
    std::fs::create_dir_all(&directory).map_err(|e| format!("OSError: {e}"))?;
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/ggml-{name}.bin",
        MODELS["repository"].as_str().unwrap(),
        MODELS["revision"].as_str().unwrap()
    );
    let mut temp =
        tempfile::NamedTempFile::new_in(&directory).map_err(|e| format!("OSError: {e}"))?;
    download_model(&url, expected_bytes, expected_hash, temp.as_file_mut())?;
    temp.as_file_mut()
        .sync_all()
        .map_err(|e| format!("OSError: {e}"))?;
    temp.persist(&path).map_err(|e| format!("OSError: {e}"))?;
    Ok(path)
}
fn verify_model(path: &Path, bytes: u64, hash: &str) -> Result<()> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("OSError: {e}"))?;
    if file.metadata().map_err(|e| format!("OSError: {e}"))?.len() != bytes {
        return Err(format!(
            "ValueError: cached Whisper model {} has the wrong size; remove the damaged cache file",
            path.display()
        )
        .into());
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("OSError: {e}"))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    if format!("{:x}", digest.finalize()) != hash {
        return Err(format!(
            "ValueError: cached Whisper model {} failed SHA-256 verification",
            path.display()
        )
        .into());
    }
    Ok(())
}
fn download_model(url: &str, bytes: u64, hash: &str, target: &mut impl Write) -> Result<()> {
    let resolver = security::SystemResolver;
    let mut transport = security::PinnedHttpTransport;
    let mut current = reqwest::Url::parse(url).map_err(|e| format!("ValueError: {e}"))?;
    for _ in 0..=10 {
        security::validate_url(current.as_str())?;
        let address = security::resolve_and_validate(
            current.host_str().ok_or("Missing model hostname")?,
            current.port_or_known_default().unwrap_or(443),
            &resolver,
        )?;
        let mut response = transport.get(&current, address, Duration::from_secs(1800))?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            current = current
                .join(
                    response
                        .location
                        .as_deref()
                        .ok_or("Missing model redirect")?,
                )
                .map_err(|e| format!("ValueError: {e}"))?;
            continue;
        }
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "HTTPError: Whisper model download returned {}",
                response.status
            )
            .into());
        }
        let mut total = 0u64;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = response
                .body
                .read(&mut buffer)
                .map_err(|e| format!("OSError: {e}"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > bytes {
                return Err("OSError: model download exceeded its declared size".into());
            }
            digest.update(&buffer[..n]);
            target
                .write_all(&buffer[..n])
                .map_err(|e| format!("OSError: {e}"))?;
        }
        if total != bytes || format!("{:x}", digest.finalize()) != hash {
            return Err(
                "ValueError: downloaded Whisper model failed its pinned size/SHA-256 check".into(),
            );
        }
        return Ok(());
    }
    Err("HTTPError: too many model download redirects".into())
}
pub struct NativeSpeech {
    pub env: Environment,
}
#[cfg(feature = "transcription")]
struct Deadline {
    started: std::time::Instant,
    limit: Duration,
}
#[cfg(feature = "transcription")]
unsafe extern "C" fn deadline_reached(data: *mut std::ffi::c_void) -> bool {
    // The immutable boxed deadline outlives the synchronous state.full call.
    let deadline = unsafe { &*data.cast::<Deadline>() };
    deadline.started.elapsed() > deadline.limit
}
impl SpeechEngine for NativeSpeech {
    fn transcribe(&self, audio: &Path, model: &str, prompt: &str) -> Result<Transcript> {
        #[cfg(feature = "transcription")]
        {
            self.infer(audio, model, prompt)
        }
        #[cfg(not(feature = "transcription"))]
        {
            let _ = (audio, model, prompt);
            Err("ImportError: this native build omits transcription; rebuild with the transcription feature".into())
        }
    }
}
#[cfg(feature = "transcription")]
impl NativeSpeech {
    fn infer(&self, audio: &Path, model: &str, prompt: &str) -> Result<Transcript> {
        use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
        if prompt.contains('\0') {
            return Err("ValueError: Whisper initial prompt contains a null byte".into());
        }
        if !audio.is_file() {
            return Err(format!("FileNotFoundError: {}", audio.display()).into());
        }
        let ffmpeg = self
            .env
            .get("GRAPHICLE_FFMPEG")
            .map(String::as_str)
            .unwrap_or("ffmpeg");
        let executable = llm_config::executable(ffmpeg, &self.env).ok_or(
            "ImportError: native audio decoding requires ffmpeg; install it or set GRAPHICLE_FFMPEG",
        )?;
        let temporary = tempfile::tempdir().map_err(|e| format!("OSError: {e}"))?;
        let raw = temporary.path().join("audio.f32le");
        let limit =
            llm_config::resolve_nonnegative(&self.env, "GRAPHICLE_MAX_AUDIO_SECONDS", 14400, true)
                .min(86400) as u64
                * 16000
                * 4;
        // File output keeps the subprocess pipe bounded. Crossing the explicit
        // capacity fails below; it never silently becomes a shortened transcript.
        let result = process::capture(
            Command::new(executable)
                .args(["-v", "error", "-nostdin", "-y", "-i"])
                .arg(audio)
                .args([
                    "-vn",
                    "-ac",
                    "1",
                    "-ar",
                    "16000",
                    "-f",
                    "f32le",
                    "-acodec",
                    "pcm_f32le",
                    "-fs",
                ])
                .arg((limit + 4).to_string())
                .arg(&raw),
            Duration::from_secs(900),
        )
        .map_err(|e| format!("OSError: ffmpeg: {e}"))?;
        if !result.success {
            return Err(format!(
                "RuntimeError: ffmpeg failed: {}",
                String::from_utf8_lossy(&result.stderr)
                    .trim()
                    .chars()
                    .take(2000)
                    .collect::<String>()
            )
            .into());
        }
        let size = raw.metadata().map_err(|e| format!("OSError: {e}"))?.len();
        if size > limit {
            return Err("ValueError: decoded audio exceeds GRAPHICLE_MAX_AUDIO_SECONDS; no transcript was written".into());
        }
        if size == 0 {
            return Err("ValueError: audio contains no decodable samples".into());
        }
        let bytes = std::fs::read(&raw).map_err(|e| format!("OSError: {e}"))?;
        if bytes.len() % 4 != 0 {
            return Err("ValueError: invalid decoded audio".into());
        }
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        if samples.iter().any(|n| !n.is_finite()) {
            return Err("ValueError: decoded audio contains nonfinite samples".into());
        }
        let path = model_path(model, &self.env)?;
        let mut context_params = WhisperContextParameters::default();
        context_params.use_gpu(false);
        let context = WhisperContext::new_with_params(&path, context_params)
            .map_err(|e| format!("RuntimeError: could not load native Whisper model: {e}"))?;
        let mut state = context
            .create_state()
            .map_err(|e| format!("RuntimeError: {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        params.set_language(None);
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_initial_prompt(prompt);
        params.set_n_threads(
            llm_config::resolve_nonnegative(
                &self.env,
                "GRAPHICLE_WHISPER_THREADS",
                std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i64,
                true,
            )
            .min(256) as i32,
        );
        let deadline = Box::new(Deadline {
            started: std::time::Instant::now(),
            limit: Duration::from_secs(llm_config::resolve_nonnegative(
                &self.env,
                "GRAPHICLE_TRANSCRIBE_TIMEOUT",
                7200,
                true,
            ) as u64),
        });
        // whisper-rs 0.16's safe abort adapter casts a boxed trait object to
        // the closure's concrete type. Use its documented raw callback instead,
        // keeping ownership here and retaining the precise pointer type.
        unsafe {
            params.set_abort_callback(Some(deadline_reached));
            params.set_abort_callback_user_data((&*deadline as *const Deadline).cast_mut().cast());
        }
        state
            .full(params, &samples)
            .map_err(|e| format!("RuntimeError: native Whisper transcription failed: {e}"))?;
        Ok(Transcript {
            segments: state.as_iter().map(|s| s.to_string()).collect(),
            language: whisper_rs::get_lang_str(state.full_lang_id_from_state())
                .unwrap_or("unknown")
                .into(),
        })
    }
}
