// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/prs.py
use crate::{
    Result,
    build::Diagnostics,
    fsutil,
    llm::{Configuration, Runtime},
    prs::{Commands, Output, PRInfo, RenderContext},
};
use serde_json::{Value, json};
use std::{io::BufRead, sync::Arc, time::Duration};

pub trait Triage: Send + Sync {
    fn resolve(&self) -> Result<(String, String)>;
    fn stream(
        &self,
        backend: &str,
        model: &str,
        prompt: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<()>;
}
pub fn resolve_triage_backend(
    config: &Configuration,
    claude_available: bool,
) -> Result<(String, String)> {
    let chosen = config
        .env
        .get("GRAPHICLE_TRIAGE_BACKEND")
        .map(|s| s.trim())
        .unwrap_or("");
    let model = |backend: &str| -> Result<String> {
        if let Some(m) = config
            .env
            .get("GRAPHICLE_TRIAGE_MODEL")
            .filter(|s| !s.is_empty())
        {
            return Ok(m.clone());
        }
        match backend {
            "claude" => Ok("claude-opus-4-7".into()),
            "kimi" => Ok("kimi-k2.6".into()),
            "openai" => Ok("gpt-4.1-mini".into()),
            "gemini" => Ok("gemini-3-flash-preview".into()),
            _ => config.model(backend),
        }
    };
    if config.backends.contains_key(chosen) {
        return Ok((chosen.into(), model(chosen)?));
    }
    for backend in ["claude", "kimi", "openai", "gemini"] {
        if !config.api_key(backend)?.is_empty() {
            return Ok((backend.into(), model(backend)?));
        }
    }
    if claude_available {
        return Ok(("claude-cli".into(), "claude-code-plan".into()));
    }
    Ok(("ollama".into(), config.model("ollama")?))
}
pub struct NativeTriage {
    pub runtime: Runtime,
    pub commands: Arc<dyn Commands>,
}
fn available(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|p| {
            #[cfg(windows)]
            {
                p.join(program).is_file()
                    || p.join(format!("{program}.cmd")).is_file()
                    || p.join(format!("{program}.exe")).is_file()
            }
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt;
                p.join(program)
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
        })
    })
}
pub fn consume_sse(
    reader: &mut dyn BufRead,
    anthropic: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut data = String::new();
    let mut line = String::new();
    fn dispatch(data: &str, anthropic: bool, on_text: &mut dyn FnMut(&str)) -> Result<bool> {
        if data.is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            return Ok(true);
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|e| format!("Invalid streamed provider response: {e}"))?;
        if event["type"] == "error" || event.get("error").is_some() {
            return Err(format!(
                "Provider streamed error: {}",
                event["error"]
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )
            .into());
        }
        if anthropic {
            if event["type"] == "content_block_delta"
                && event["delta"]["type"] == "text_delta"
                && let Some(s) = event["delta"]["text"].as_str()
            {
                on_text(s);
            }
        } else if let Some(s) = event["choices"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|c| c["delta"]["content"].as_str())
        {
            on_text(s);
        }
        Ok(false)
    }
    loop {
        line.clear();
        let count = reader.read_line(&mut line).map_err(crate::Error::from)?;
        if count == 0 {
            dispatch(data.trim_end_matches('\n'), anthropic, on_text)?;
            return Ok(());
        }
        if line.len() + data.len() > 1024 * 1024 {
            return Err("Provider stream event exceeds one MiB".into());
        }
        let value = line.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            if dispatch(data.trim_end_matches('\n'), anthropic, on_text)? {
                return Ok(());
            }
            data.clear();
        } else if let Some(value) = value.strip_prefix("data:") {
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            data.push('\n');
        }
    }
}
impl Triage for NativeTriage {
    fn resolve(&self) -> Result<(String, String)> {
        resolve_triage_backend(&self.runtime.config, available("claude"))
    }
    fn stream(
        &self,
        backend: &str,
        model: &str,
        prompt: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<()> {
        if backend == "claude-cli" {
            #[cfg(not(windows))]
            let executable = "claude";
            #[cfg(windows)]
            let executable = if available("claude.cmd") {
                "claude.cmd"
            } else {
                "claude"
            };
            let output = self.commands.run(
                executable,
                &["-p".into(), "--no-session-persistence".into()],
                Duration::from_secs(120),
                Some(prompt),
            )?;
            if output.code != Some(0) {
                return Err(format!(
                    "claude -p failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                        .trim_matches(crate::dedup_strings::whitespace)
                        .chars()
                        .take(300)
                        .collect::<String>()
                )
                .into());
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let parsed = serde_json::from_str::<Value>(&text).ok();
            let result = parsed
                .as_ref()
                .and_then(|v| v.get("result"))
                .filter(|v| crate::build::truthy(v));
            if let Some(result) = result {
                on_text(result.as_str().ok_or("Claude CLI result must be text")?);
            } else {
                on_text(&text);
            }
            return Ok(());
        }
        if !["claude", "kimi", "openai", "gemini", "ollama"].contains(&backend) {
            // Use the already-qualified generic adapter for additional native
            // backends instead of the upstream function's silent no-op.
            let mut usage = crate::llm::Usage::default();
            let mut messages = Diagnostics::default();
            let answer = self
                .runtime
                .call(
                    prompt,
                    backend,
                    Some(model),
                    1024,
                    &mut usage,
                    &mut messages,
                )
                .map_err(crate::Error::from)?;
            on_text(&answer);
            return Ok(());
        }
        let config = self.runtime.config.backend(backend)?;
        let key = self.runtime.config.api_key(backend)?;
        let anthropic = backend == "claude";
        let url = if anthropic {
            let base = self
                .runtime
                .config
                .env
                .get("ANTHROPIC_BASE_URL")
                .filter(|s| !s.is_empty())
                .map(String::as_str)
                .unwrap_or(
                    config["base_url"]
                        .as_str()
                        .unwrap_or("https://api.anthropic.com"),
                );
            format!("{}/v1/messages", base.trim_end_matches('/'))
        } else {
            format!(
                "{}/chat/completions",
                config["base_url"]
                    .as_str()
                    .unwrap_or("")
                    .trim_end_matches('/')
            )
        };
        let body = json!({"model":model,"max_tokens":1024,"stream":true,"messages":[{"role":"user","content":prompt}]});
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(crate::Error::from)?;
        for attempt in 0..3 {
            let mut request = client
                .post(&url)
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).unwrap());
            if anthropic {
                request = request
                    .header("x-api-key", &key)
                    .header("anthropic-version", "2023-06-01");
            } else {
                request = request.bearer_auth(if key.is_empty() { "ollama" } else { &key });
            }
            match request.send() {
                Ok(response) => {
                    let status = response.status();
                    let retry =
                        [408, 409, 429].contains(&status.as_u16()) || status.is_server_error();
                    if retry && attempt < 2 {
                        self.runtime
                            .transport
                            .sleep(Duration::from_millis(500u64 << attempt));
                        continue;
                    }
                    if !status.is_success() {
                        return Err(format!("Triage provider returned HTTP {status}").into());
                    }
                    return consume_sse(&mut std::io::BufReader::new(response), anthropic, on_text);
                }
                Err(_) if attempt < 2 => self
                    .runtime
                    .transport
                    .sleep(Duration::from_millis(500u64 << attempt)),
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }
}
pub fn triage_prompt(prs: &[&PRInfo], now: i64) -> String {
    let mut lines = Vec::new();
    for pr in prs {
        let radius = pr.blast_radius();
        let impact = if radius.is_empty() {
            String::new()
        } else {
            format!(", blast_radius={radius}")
        };
        lines.push(format!(
            "PR #{} [{}] CI={} review={} age={}d author={}{impact}\n  title: {}",
            pr.number,
            pr.status(now),
            pr.ci_status,
            if pr.review_decision.is_empty() {
                "none"
            } else {
                &pr.review_decision
            },
            pr.days_old(now),
            pr.author,
            pr.title
        ));
    }
    format!(
        "You are a senior engineer helping triage a PR review queue. Given these open PRs, rank them by review priority for the repo maintainer. For each PR give: priority number, one sentence on what action to take and why. Be direct and specific. Format each as: #<number> — <action>.\n\n{}",
        lines.join("\n\n")
    )
}
pub fn triage_with_opus(
    prs: &[PRInfo],
    base: &str,
    triage: &dyn Triage,
    c: &RenderContext,
) -> Output {
    let candidates: Vec<_> = prs
        .iter()
        .filter(|p| {
            p.base_branch == base && !["WRONG-BASE", "STALE"].contains(&p.status(c.now_micros))
        })
        .collect();
    if candidates.is_empty() {
        return Output {
            stdout: format!("{}\n", c.color("2", "  No actionable PRs to triage.")),
            ..Default::default()
        };
    }
    let (backend, model) = match triage.resolve() {
        Ok(v) => v,
        Err(e) => {
            return Output {
                stderr: format!(
                    "{}\n",
                    c.color("31", &format!("  Could not resolve triage backend: {e}"))
                ),
                code: 1,
                ..Default::default()
            };
        }
    };
    let mut out = Output {
        stdout: format!(
            "\n{}{}\n\n",
            c.color("1", "  Triage"),
            c.color("2", &format!(" ({backend} / {model})"))
        ),
        ..Default::default()
    };
    let prompt = triage_prompt(&candidates, c.now_micros);
    if backend == "claude-cli" {
        let mut answer = String::new();
        match triage.stream(&backend, &model, &prompt, &mut |s| answer.push_str(s)) {
            Ok(()) => {
                for line in fsutil::split_lines(&answer) {
                    out.stdout.push_str(&format!("  {line}\n"));
                }
                out.stdout.push('\n');
            }
            Err(e) if e.to_string().starts_with("claude -p failed:") => out
                .stderr
                .push_str(&format!("{}\n", c.color("31", &format!("  {e}")))),
            Err(e) => out.stderr.push_str(&format!(
                "\n\n  {}\n",
                c.color("31", &format!("Triage failed: {e}"))
            )),
        }
    } else {
        out.stdout.push_str("  ");
        match triage.stream(&backend, &model, &prompt, &mut |s| {
            out.stdout.push_str(&s.replace('\n', "\n  "))
        }) {
            Ok(()) => out.stdout.push_str("\n\n"),
            Err(e) => out.stderr.push_str(&format!(
                "\n\n  {}\n",
                c.color("31", &format!("Triage failed: {e}"))
            )),
        }
    }
    out
}
