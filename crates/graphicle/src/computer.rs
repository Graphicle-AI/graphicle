//! Computer use (`graphicle see`): a model drives a browser that Graphicle
//! runs, through the computer tool of the Meta Model API's Responses API,
//! until the screen a task describes shows, or it cannot get there.
//!
//! The loop follows <https://dev.meta.ai/docs/computer-use.md>:
//! 1. The first request holds the task and a screenshot, with the tool
//!    `{"type":"computer"}`.
//! 2. A reply's `computer_call` carries a batch of actions (`click`,
//!    `double_click`, `drag`, `keypress`, `move`, `scroll`, `type`,
//!    `screenshot`, `wait`) in the latest screenshot's pixels.
//! 3. The driver runs them in order, stops at the first that fails, and
//!    takes one screenshot of the viewport ([`Screen`]).
//! 4. The screenshot goes back as a `computer_call_output`, with the safety
//!    checks acknowledged for that call.
//! 5. A reply without a `computer_call` ends the run: its message is the
//!    model's answer, a JSON object that says what the last screen shows.
//!
//! **State is replayed** (`store: false`): each request holds the first
//! message and every call and output so far, each call exactly as the model
//! returned it, so a `meta_safety_replay_receipt` goes back unchanged. The
//! requests ask for `include: ["reasoning.encrypted_content"]`, and the
//! reasoning items that come back with it go back, unchanged, before their
//! call, so the model keeps its reasoning from step to step.
//! Replay holds at most 29 pairs and `truncation` is not sent, so a run takes
//! at most [`MAX_STEPS`] model calls ([`DEFAULT_STEPS`] unless asked).
//!
//! **Safety:** a pending safety check stops the run, unless the person
//! passed `--acknowledge-safety-checks` (meant for their own sandboxed app
//! only); then each acknowledgement goes in the step log and on standard
//! error. The driver gets no model key. Graphicle's driver
//! (`demo/browser_driver.py`) loads only the app's origin.
//!
//! **Money:** every call goes in the spend log as `see`, one line per step,
//! under the profile's caps (`max_cost_per_call`, `max_cost_per_day`), with
//! each step's worst case (its input, none of it cached, and the step's
//! output limit) checked against the run's `--max-cost` too. A run needs a
//! priced profile, so that the caps can work.
//!
//! The pieces are public for other callers: [`body`], [`parse`],
//! [`verdict`], the [`Screen`] trait and the [`Driver`] process.
use crate::model::{self, Profile};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Model calls a run takes at most: stateless replay holds 29 call and
/// output pairs.
pub const MAX_STEPS: u64 = 29;
/// Model calls a run takes unless asked for another number.
pub const DEFAULT_STEPS: u64 = 25;
/// Output tokens a step may use unless asked for another number.
pub const DEFAULT_OUTPUT_TOKENS: u64 = 8_192;
/// Input tokens counted in a step's worst case for a screenshot of
/// [`IMAGE_AREA`] pixels (Meta documents about 1,300-1,500 for a 1280-pixel
/// image); a larger one counts in proportion to its area, a smaller one as
/// this much.
pub const IMAGE_TOKENS: u64 = 1_600;
/// The pixel area [`IMAGE_TOKENS`] stands for: 1280 by 800.
pub const IMAGE_AREA: u64 = 1_280 * 800;
/// Base64 characters of a data URL read at most to find its image's size
/// (a JPEG's size can sit after its other headers).
const HEADER_CHARS: usize = 87_384;
/// The largest screenshot the API takes, decoded.
pub const MAX_IMAGE_BYTES: usize = 25 << 20;
/// How long one model call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest line read from a driver (a base64 screenshot and its fields).
const MAX_REPLY_BYTES: u64 = 48 << 20;
/// Characters of the model's last answer kept in the report.
const ANSWER_CHARS: usize = 4_000;
/// The largest task file read.
const MAX_TASK_BYTES: u64 = 1 << 20;

/// The instructions of every step.
pub const INSTRUCTIONS: &str = "You test a web application in a browser for a developer, with the computer tool. The task gives an issue and what to look for. Each turn, look at the latest screenshot and act on it; coordinates are in that screenshot's pixels from its top left corner. Stay inside the application: do not open other sites, sign in, buy, download, upload or send anything. When the screen shows what the task asks you to look for, or you cannot get there, stop acting and reply with one JSON object and nothing else: {\"shown\": \"bug\" or \"expected\" or \"neither\", \"point\": {\"x\": X, \"y\": Y} or null, \"summary\": \"...\"}. shown says what the last screenshot shows: the issue's bug, the expected behaviour, or neither; point is a point on the element of the last screenshot that shows it; summary says in one or two sentences what you did and what the screen shows.";

/// What a run looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookFor {
    /// Reproduce the issue until its bug shows.
    Bug,
    /// Check that the expected behaviour shows now (after a fix).
    Expected,
}

impl LookFor {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "bug" => Ok(Self::Bug),
            "expected" => Ok(Self::Expected),
            _ => Err(Error::bad("--look-for takes bug or expected")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Expected => "expected",
        }
    }
}

/// The first message's text: what to look for, then the task.
pub fn task_text(look_for: LookFor, task: &str) -> String {
    let goal = match look_for {
        LookFor::Bug => {
            "Reproduce this issue: follow its steps in the application until its bug shows on the screen, then answer."
        }
        LookFor::Expected => {
            "The code was changed to fix this issue. Follow its steps in the application and check whether the screen now shows the expected behaviour instead of the bug, then answer."
        }
    };
    format!("{goal}\n\n{}", task.trim())
}

/// The computer tool, as Meta documents it.
pub fn tool() -> Value {
    json!({"type":"computer"})
}

/// The first input item: the task and the first screenshot.
pub fn first_message(text: &str, image_url: &str) -> Value {
    json!({"role":"user","content":[{"type":"input_text","text":text},
        {"type":"input_image","image_url":image_url}]})
}

/// The output of `call_id`: its screenshot and the safety checks
/// acknowledged for it.
pub fn call_output(call_id: &str, acknowledged: &[Value], image_url: &str) -> Value {
    json!({"type":"computer_call_output","call_id":call_id,
        "acknowledged_safety_checks":acknowledged,
        "output":{"type":"computer_screenshot","image_url":image_url}})
}

/// One step's request: the instructions, the replayed input, the computer
/// tool, no stored state, and the step's output limit. A profile with the
/// prompt cache on sends a cache key, as `ask --pack` does; its reasoning
/// effort goes in too.
pub fn body(
    profile: &Profile,
    instructions: &str,
    input: &[Value],
    output_tokens: usize,
    cache_key: Option<&str>,
) -> Value {
    let mut body = json!({"model":profile.model,"instructions":instructions,"input":input,
        "tools":[tool()],"store":false,"include":["reasoning.encrypted_content"],
        "max_output_tokens":output_tokens});
    if let Some(effort) = &profile.reasoning_effort {
        body["reasoning"] = json!({"effort":effort});
    }
    if let Some(key) = cache_key.filter(|_| model::prompt_cache_on(profile)) {
        body["prompt_cache_key"] = json!(key);
        body["prompt_cache_retention"] = json!(model::PROMPT_CACHE_RETENTION);
    }
    body
}

/// What one reply holds.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// `completed`, `incomplete` or another status (`completed` when none).
    pub status: String,
    /// Why it is incomplete (`max_output_tokens`).
    pub incomplete: Option<String>,
    /// The error of a reply whose status is `failed` or `cancelled`.
    pub error: Option<String>,
    /// Its `computer_call` items, exactly as returned.
    pub calls: Vec<Value>,
    /// Its reasoning items that carry `encrypted_content`, exactly as
    /// returned, to replay before its calls (one without it cannot be
    /// replayed when nothing is stored).
    pub reasoning: Vec<Value>,
    /// The text of its messages.
    pub text: String,
    pub completion: model::Completion,
}

/// A reply of the Responses API with the computer tool.
pub fn parse(value: &Value, elapsed_ms: u64, fallback_model: &str) -> Turn {
    let mut turn = Turn {
        status: value["status"].as_str().unwrap_or("completed").to_string(),
        incomplete: value["incomplete_details"]["reason"]
            .as_str()
            .map(str::to_string),
        error: value["error"]["message"]
            .as_str()
            .or(value["error"].as_str())
            .map(str::to_string),
        ..Turn::default()
    };
    for item in value["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("computer_call") => turn.calls.push(item.clone()),
            Some("reasoning") if item["encrypted_content"].is_string() => {
                turn.reasoning.push(item.clone())
            }
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if part["type"] == "output_text"
                        && let Some(text) = part["text"].as_str()
                    {
                        turn.text.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }
    let usage = &value["usage"];
    turn.completion = model::Completion {
        text: turn.text.clone(),
        input_tokens: usage["input_tokens"].as_u64(),
        output_tokens: usage["output_tokens"].as_u64(),
        cached_tokens: usage["input_tokens_details"]["cached_tokens"].as_u64(),
        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64(),
        elapsed_ms,
        model: value["model"]
            .as_str()
            .unwrap_or(fallback_model)
            .to_string(),
    };
    turn
}

/// A call's actions: its `actions`, or one `action`.
pub fn actions_of(call: &Value) -> Vec<Value> {
    match (&call["actions"], &call["action"]) {
        (Value::Array(actions), _) => actions.clone(),
        (_, action @ Value::Object(_)) => vec![action.clone()],
        _ => Vec::new(),
    }
}

/// A call's pending safety checks.
pub fn safety_checks(call: &Value) -> Vec<Value> {
    call["pending_safety_checks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// One action in a few words, for the step log's readers.
pub fn describe(action: &Value) -> String {
    let at = |a: &Value| match (a["x"].as_f64(), a["y"].as_f64()) {
        (Some(x), Some(y)) => format!(" ({x}, {y})"),
        _ => String::new(),
    };
    let kind = action["type"].as_str().unwrap_or("?");
    match kind {
        "click" => {
            let button = action["button"].as_str().unwrap_or("left");
            if button == "left" {
                format!("click{}", at(action))
            } else {
                format!("{button} click{}", at(action))
            }
        }
        "double_click" | "move" => format!("{}{}", kind.replace('_', " "), at(action)),
        "scroll" => format!(
            "scroll{} by ({}, {})",
            at(action),
            action["scroll_x"].as_f64().unwrap_or(0.0),
            action["scroll_y"].as_f64().unwrap_or(0.0)
        ),
        "type" => {
            let text: String = action["text"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            format!("type {text:?}")
        }
        "keypress" => {
            let keys: Vec<&str> = action["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            format!("keys {}", keys.join("+"))
        }
        "drag" => format!(
            "drag through {} points",
            action["path"].as_array().map_or(0, Vec::len)
        ),
        other => other.to_string(),
    }
}

/// The model's answer at the end of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// `bug`, `expected` or `neither`.
    pub shown: String,
    /// A point on the element that shows it, inside the last screenshot.
    pub point: Option<(u32, u32)>,
    pub summary: String,
}

/// The answer in the model's last text, checked against the last
/// screenshot's size: none when it is not the JSON object asked for.
pub fn verdict(text: &str, width: u32, height: u32) -> Option<Verdict> {
    let value = crate::long_context::json_in(text).ok()?;
    let shown = value["shown"].as_str()?.trim().to_ascii_lowercase();
    if !matches!(shown.as_str(), "bug" | "expected" | "neither") {
        return None;
    }
    let point = match (value["point"]["x"].as_f64(), value["point"]["y"].as_f64()) {
        (Some(x), Some(y))
            if x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height) =>
        {
            Some((x as u32, y as u32))
        }
        _ => None,
    };
    let summary = value["summary"].as_str().unwrap_or("").trim().to_string();
    Some(Verdict {
        shown,
        point,
        summary,
    })
}

// ---------------------------------------------------------------- screen

/// The browser (or desktop) a run drives: it answers one request at a time
/// with one reply, in the driver protocol of `demo/browser_driver.py`:
/// `{"op":"screenshot"}` and `{"op":"act","actions":[...]}` get
/// `{"ok":true,"done":N,"error":...,"url":...,"blocked":N,"png":"<base64>"}`.
pub trait Screen {
    fn ask(&mut self, request: &Value) -> Result<Value>;
}

/// A screenshot from a driver's reply.
#[derive(Clone, Debug)]
pub struct Shot {
    pub bytes: Vec<u8>,
    pub image: crate::pixel::ImageInfo,
    /// The reply without its image: `done`, `error`, `url`, `blocked`.
    pub reply: Value,
}

impl Shot {
    pub fn data_url(&self) -> String {
        crate::pixel::data_url(&self.image, &self.bytes)
    }
}

/// The screenshot in a driver's reply, checked: a PNG or JPEG of at most
/// [`MAX_IMAGE_BYTES`].
pub fn shot_of(mut reply: Value) -> Result<Shot> {
    use base64::Engine as _;
    if reply["ok"] != true {
        return Err(Error::bad(format!(
            "The driver refused the request: {}",
            reply["error"].as_str().unwrap_or("no reason given")
        )));
    }
    let encoded = reply
        .get("png")
        .or_else(|| reply.get("image"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::bad("The driver's reply holds no screenshot"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| Error::bad("The driver's screenshot is not base64"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::bad("The driver's screenshot is over 25 MiB"));
    }
    let image = crate::pixel::image_info(&bytes)
        .filter(|i| matches!(i.mime, "image/png" | "image/jpeg"))
        .ok_or_else(|| Error::bad("The driver's screenshot is not a PNG or JPEG"))?;
    if let Some(fields) = reply.as_object_mut() {
        fields.remove("png");
        fields.remove("image");
        fields.remove("ok");
    }
    Ok(Shot {
        bytes,
        image,
        reply,
    })
}

/// A driver process: a command run with `sh -c` (`cmd /C` on Windows),
/// one JSON request per line on its standard input and one reply per line
/// on its standard output; its standard error goes to a log file. It gets
/// no model key. Dropping it asks it to close, then stops it.
pub struct Driver {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: mpsc::Receiver<std::io::Result<String>>,
    timeout: Duration,
}

impl Driver {
    pub fn start(
        command: &str,
        log: &Path,
        timeout: Duration,
        scrub: &crate::fix::Scrub,
    ) -> Result<Self> {
        #[cfg(unix)]
        let mut process = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        #[cfg(not(unix))]
        let mut process = {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        };
        let errors = std::fs::File::create(log)?;
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(errors));
        for name in &scrub.remove {
            process.env_remove(name);
        }
        let mut child = process
            .spawn()
            .map_err(|e| Error::bad(format!("Cannot start the driver: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::bad("The driver has no standard output"))?;
        let (send, replies) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match (&mut reader).take(MAX_REPLY_BYTES).read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if send.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = send.send(Err(error));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            replies,
            timeout,
        })
    }
}

impl Screen for Driver {
    fn ask(&mut self, request: &Value) -> Result<Value> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::bad("The driver is closed"))?;
        writeln!(stdin, "{request}")
            .and_then(|()| stdin.flush())
            .map_err(|e| Error::bad(format!("The driver stopped reading: {e}")))?;
        match self.replies.recv_timeout(self.timeout) {
            Ok(Ok(line)) => serde_json::from_str(line.trim())
                .map_err(|_| Error::bad("The driver's reply is not one JSON object on a line")),
            Ok(Err(error)) => Err(Error::bad(format!("Cannot read the driver: {error}"))),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::bad(format!(
                "The driver did not answer within {} s",
                self.timeout.as_secs()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::bad("The driver ended; its log says why"))
            }
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = writeln!(stdin, "{}", json!({"op":"close"}));
        }
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- the run

/// `graphicle see`'s arguments.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The task: the issue, with its steps and what goes wrong; `-` reads it
    /// from standard input.
    #[arg(required_unless_present = "task_file", conflicts_with = "task_file")]
    pub task: Option<String>,
    /// Read the task from this file (at most 1 MiB).
    #[arg(long, value_name = "FILE")]
    pub task_file: Option<PathBuf>,
    /// The driver: a command, run with `sh -c`, that speaks the driver
    /// protocol on its standard input and output, such as
    /// `python3 demo/browser_driver.py --url http://HOST:PORT/`.
    #[arg(long, value_name = "CMD")]
    pub driver: String,
    /// What the run looks for: the issue's bug, or the expected behaviour
    /// (after a fix).
    #[arg(long, value_parser = ["bug", "expected"], default_value = "bug")]
    pub look_for: String,
    /// Model calls at most, 1 to 29 (stateless replay holds 29 pairs).
    #[arg(long, default_value_t = DEFAULT_STEPS, value_parser = clap::value_parser!(u64).range(1..=MAX_STEPS))]
    pub steps: u64,
    /// The run's cap in US dollars at the profile's price: a step whose
    /// worst case would bring the run over it is not sent.
    #[arg(long, value_name = "USD", default_value_t = 3.0)]
    pub max_cost: f64,
    /// Output tokens a step may use at most.
    #[arg(long, value_name = "TOKENS", default_value_t = DEFAULT_OUTPUT_TOKENS, value_parser = clap::value_parser!(u64).range(256..=131_072))]
    pub output_tokens: u64,
    /// Acknowledge the model's pending safety checks, each one logged,
    /// instead of stopping the run. Only on your own sandboxed app.
    #[arg(long)]
    pub acknowledge_safety_checks: bool,
    /// The folder for the screenshots, the step log and the driver's log
    /// [default: graphicle-see-<time> in this folder].
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Seconds the driver may take for one request.
    #[arg(long, value_name = "SECONDS", default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3_600))]
    pub driver_timeout: u64,
    /// The configured model profile: Muse Wonder (recommended) or Muse
    /// Spark 1.3 on the Meta Model API [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// Print the report as JSON (the default when the output is not a
    /// terminal).
    #[arg(long)]
    pub json: bool,
}

/// The task's text from the arguments.
fn task_of(args: &Args) -> Result<String> {
    let text = match (&args.task, &args.task_file) {
        (Some(text), None) if text == "-" => {
            let mut text = String::new();
            std::io::stdin()
                .take(MAX_TASK_BYTES)
                .read_to_string(&mut text)?;
            text
        }
        (Some(text), None) => text.clone(),
        (None, Some(path)) => {
            let mut text = String::new();
            std::fs::File::open(path)
                .map_err(|e| Error::bad(format!("Cannot read {}: {e}", path.display())))?
                .take(MAX_TASK_BYTES)
                .read_to_string(&mut text)?;
            text
        }
        _ => return Err(Error::bad("Give the task, or --task-file FILE")),
    };
    if text.trim().is_empty() {
        return Err(Error::bad("The task is empty"));
    }
    Ok(text)
}

/// A step's input tokens at most: the text at three bytes a token, and
/// [`image_tokens`] for each screenshot.
/// The input tokens a step's worst case counts for an image data URL: its
/// area over [`IMAGE_AREA`] times [`IMAGE_TOKENS`], at least
/// [`IMAGE_TOKENS`]; an image whose size cannot be read counts as one of
/// 2560 by 1600 pixels.
pub fn image_tokens(url: &str) -> u64 {
    use base64::Engine as _;
    let payload = url.split_once(',').map_or("", |(_, data)| data);
    let size = [64, HEADER_CHARS].iter().find_map(|&chars| {
        let head = &payload[..payload.len().min(chars) / 4 * 4];
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(head)
            .ok()?;
        crate::pixel::image_info(&bytes).map(|i| u64::from(i.width) * u64::from(i.height))
    });
    let area = size.unwrap_or(4 * IMAGE_AREA);
    (IMAGE_TOKENS * area).div_ceil(IMAGE_AREA).max(IMAGE_TOKENS)
}

pub fn input_estimate(instructions: &str, input: &[Value]) -> u64 {
    fn walk(value: &Value, images: &mut u64, bytes: &mut u64) {
        match value {
            Value::String(text) if text.starts_with("data:image/") => *images += image_tokens(text),
            Value::String(text) => *bytes += text.len() as u64,
            Value::Array(items) => items.iter().for_each(|v| walk(v, images, bytes)),
            Value::Object(fields) => {
                for (key, v) in fields {
                    *bytes += key.len() as u64 + 4;
                    walk(v, images, bytes);
                }
            }
            other => *bytes += other.to_string().len() as u64,
        }
    }
    let mut images = 0u64;
    let mut bytes = instructions.len() as u64;
    input.iter().for_each(|v| walk(v, &mut images, &mut bytes));
    bytes.div_ceil(3) + images
}

/// What a run looked for, found and spent.
struct Run<'a> {
    store: &'a crate::store::Store,
    name: String,
    /// The profile with the step's output limit, for the caps.
    profile: Profile,
    out: PathBuf,
    max_cost: f64,
    spent: f64,
    calls: usize,
    human: bool,
    cache_key: Option<String>,
}

impl Run<'_> {
    fn save(&self, file: &str, bytes: &[u8]) -> Result<()> {
        std::fs::write(self.out.join(file), bytes)?;
        Ok(())
    }

    /// One model call under the caps: the reply (or why there is none) and
    /// its log entry; `Err` when a cap refuses it.
    async fn call(&mut self, step: u64, input: &[Value]) -> Result<(Result<Turn>, Value)> {
        let estimate = input_estimate(INSTRUCTIONS, input);
        let price = self.profile.price.clone().ok_or_else(|| {
            Error::bad("graphicle see needs the profile's price to keep the run under --max-cost")
        })?;
        let worst = price.cost(estimate, 0, self.profile.output_limit as u64);
        if self.spent + worst > self.max_cost {
            return Err(Error::bad(format!(
                "Refused before sending step {step}: the run has cost {} and this step could add up to {} ({} input tokens, none cached, and {} output tokens at most), over --max-cost {}",
                crate::spend::usd(self.spent),
                crate::spend::usd(worst),
                crate::terminal::count(estimate),
                crate::terminal::count(self.profile.output_limit as u64),
                crate::spend::usd(self.max_cost)
            )));
        }
        let held = crate::spend::reserve(
            &self.store.directory,
            &self.name,
            &self.profile,
            estimate,
            chrono::Utc::now(),
        )?;
        let request = body(
            &self.profile,
            INSTRUCTIONS,
            input,
            self.profile.output_limit,
            self.cache_key.as_deref(),
        );
        let start = Instant::now();
        let reply = model::post_json(&self.profile, "responses", &request, CALL_TIMEOUT).await;
        let elapsed = start.elapsed().as_millis() as u64;
        let turn = reply.map(|value| parse(&value, elapsed, &self.profile.model));
        let completion = match &turn {
            Ok(turn) => Ok(turn.completion.clone()),
            Err(error) => Err(Error::bad(error.message.clone())),
        };
        let call = crate::spend::Call {
            command: "see",
            profile: &self.name,
            settings: &self.profile,
            project: None,
            commit: None,
            prompt_cache_key: self.cache_key.as_deref(),
        };
        let mut entry = crate::spend::entry(&call, &completion, chrono::Utc::now());
        entry["step"] = json!(step);
        if let Err(error) = crate::spend::settle(&self.store.directory, &held, &mut entry) {
            eprintln!("graphicle: could not write the spend log: {error}");
        }
        self.calls += 1;
        self.spent += entry["cost_usd"]
            .as_f64()
            .or(entry["worst_cost_usd"].as_f64())
            .unwrap_or(0.0);
        Ok((turn, entry))
    }
}

/// The report as a person reads it.
fn report_text(report: &Value) -> String {
    let mut out = format!(
        "Result: {} after {} model calls ({} at the configured price; cap {}).\n",
        report["result"].as_str().unwrap_or("?"),
        report["calls"],
        crate::spend::usd(report["cost_usd"].as_f64().unwrap_or(0.0)),
        crate::spend::usd(report["max_cost_usd"].as_f64().unwrap_or(0.0))
    );
    if let Some(summary) = report["summary"].as_str().filter(|s| !s.is_empty()) {
        out.push_str(&format!("The model: {summary}\n"));
    }
    if let (Some(x), Some(y)) = (report["point"]["x"].as_u64(), report["point"]["y"].as_u64()) {
        out.push_str(&format!(
            "It points at ({x}, {y}) on {}: `graphicle pixel {} --at {x},{y}` finds the code that draws it.\n",
            report["last_screenshot"].as_str().unwrap_or(""),
            report["last_screenshot_path"].as_str().unwrap_or("")
        ));
    }
    for step in report["steps"].as_array().into_iter().flatten() {
        let actions: Vec<String> = step["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(describe)
            .collect();
        out.push_str(&format!(
            "  {:>2}. {}{}\n",
            step["step"],
            if actions.is_empty() {
                "answer".to_string()
            } else {
                actions.join(", ")
            },
            step["driver"]["error"]
                .as_str()
                .map(|e| format!(" (stopped: {e})"))
                .unwrap_or_default()
        ));
    }
    for ack in report["acknowledged"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "Acknowledged safety check at step {}: {} ({})\n",
            ack["step"],
            ack["check"]["message"].as_str().unwrap_or(""),
            ack["check"]["code"].as_str().unwrap_or("")
        ));
    }
    out.push_str(&format!(
        "Screenshots and the step log: {}\n",
        report["out"].as_str().unwrap_or("")
    ));
    out
}

/// `graphicle see`: run the loop and report.
pub async fn see(store: &crate::store::Store, config: &model::Config, args: Args) -> Result<()> {
    let look_for = LookFor::parse(&args.look_for)?;
    let task = task_of(&args)?;
    if !(args.max_cost.is_finite() && args.max_cost > 0.0) {
        return Err(Error::bad(
            "--max-cost must be a positive number of US dollars",
        ));
    }
    let configuration = match crate::local::session(store, false) {
        Ok(session) => crate::settings::configuration(store, &session.workspace, config)?,
        Err(_) => config.clone(),
    };
    let (name, profile) = configuration
        .profile(args.model.as_deref())
        .map_err(|_| {
            Error::bad(match &args.model {
                Some(name) => {
                    format!("No model profile named {name}: graphicle config show lists them")
                }
                None => "No answer model: choose Muse Wonder with `graphicle config set model meta:muse-wonder-1.0`".into(),
            })
        })?;
    let (name, mut profile) = (name.to_string(), profile.clone());
    if !matches!(profile.protocol.as_str(), "responses" | "azure_responses") {
        return Err(Error::bad(
            "Computer use needs a Responses API profile: Muse Wonder or Muse Spark 1.3 on the Meta Model API",
        ));
    }
    if profile.price.is_none() {
        return Err(Error::bad(format!(
            "graphicle see needs the profile's price to keep the run under --max-cost: set models.{name}.price"
        )));
    }
    profile.output_limit = (args.output_tokens as usize).min(profile.output_limit.max(256));
    let human = crate::terminal::human(args.json);
    let started = chrono::Utc::now();
    let out = args.out.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "graphicle-see-{}",
            started.format("%Y%m%dT%H%M%SZ")
        ))
    });
    std::fs::create_dir_all(&out)
        .map_err(|e| Error::bad(format!("Cannot make {}: {e}", out.display())))?;
    let out = out.canonicalize().unwrap_or(out);
    let scrub = crate::fix::Scrub::of(&configuration);
    let mut screen = Driver::start(
        &args.driver,
        &out.join("driver.log"),
        Duration::from_secs(args.driver_timeout),
        &scrub,
    )?;
    let cache_key = format!(
        "graphicle-see:{}-{}",
        started.format("%Y%m%dT%H%M%S"),
        std::process::id()
    );
    let mut run = Run {
        store,
        name,
        profile,
        out: out.clone(),
        max_cost: args.max_cost,
        spent: 0.0,
        calls: 0,
        human,
        cache_key: Some(cache_key),
    };
    let report = drive(
        &mut run,
        &mut screen,
        look_for,
        &task,
        args.steps,
        args.acknowledge_safety_checks,
    )
    .await?;
    drop(screen);
    std::fs::write(
        out.join("see.json"),
        serde_json::to_string_pretty(&report)? + "\n",
    )?;
    if human {
        print!("{}", crate::fix::printable(&report_text(&report)));
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    let wanted = match look_for {
        LookFor::Bug => "reproduced",
        LookFor::Expected => "expected shown",
    };
    match report["result"].as_str() {
        Some(result) if result == wanted => Ok(()),
        other => Err(Error::bad(format!(
            "see ended with the result {:?}; the screenshots and step log are in {}",
            other.unwrap_or("unknown"),
            out.display()
        ))),
    }
}

/// The loop itself, on any [`Screen`]: the report.
async fn drive(
    run: &mut Run<'_>,
    screen: &mut dyn Screen,
    look_for: LookFor,
    task: &str,
    steps: u64,
    acknowledge: bool,
) -> Result<Value> {
    let mut log = std::fs::File::create(run.out.join("steps.jsonl"))?;
    let first = shot_of(screen.ask(&json!({"op":"screenshot"}))?)?;
    run.save("step-00.png", &first.bytes)?;
    let size = (first.image.width, first.image.height);
    let mut input = vec![first_message(&task_text(look_for, task), &first.data_url())];
    let mut last = ("step-00.png".to_string(), first.image);
    let mut records: Vec<Value> = Vec::new();
    let mut acknowledged_all: Vec<Value> = Vec::new();
    let mut usage: Vec<Value> = Vec::new();
    let mut result = "step cap reached".to_string();
    let mut stopped: Option<String> = None;
    let mut answer: Option<(String, Option<Verdict>)> = None;
    'steps: for step in 1..=steps {
        let (turn, entry) = match run.call(step, &input).await {
            Ok(called) => called,
            Err(refused) => {
                result = "refused by a cap".into();
                stopped = Some(refused.message);
                break;
            }
        };
        usage.push(entry.clone());
        let turn = match turn {
            Ok(turn) => turn,
            Err(error) => {
                result = "provider error".into();
                stopped = Some(error.message);
                break;
            }
        };
        if matches!(turn.status.as_str(), "failed" | "cancelled") {
            result = "provider error".into();
            stopped = Some(format!(
                "The reply's status is {}: {}",
                turn.status,
                turn.error.as_deref().unwrap_or("no reason given")
            ));
            break;
        }
        if turn.status == "incomplete" {
            result = "incomplete".into();
            stopped = Some(format!(
                "The reply stopped early ({}): raise --output-tokens",
                turn.incomplete.as_deref().unwrap_or("no reason given")
            ));
            break;
        }
        if turn.calls.is_empty() {
            let verdict = verdict(&turn.text, last.1.width, last.1.height);
            result = match (verdict.as_ref().map(|v| v.shown.as_str()), look_for) {
                (Some("bug"), LookFor::Bug) => "reproduced",
                (Some("expected"), LookFor::Expected) => "expected shown",
                (Some("bug"), LookFor::Expected) => "bug still shown",
                (Some(_), _) => "not shown",
                (None, _) => "unclear answer",
            }
            .to_string();
            let record = json!({"step":step,"actions":[],"answer":true,"usage":entry});
            writeln!(log, "{record}")?;
            records.push(record);
            answer = Some((turn.text.clone(), verdict));
            break;
        }
        for (index, call) in turn.calls.iter().enumerate() {
            let call_id = call["call_id"].as_str().unwrap_or("").to_string();
            if call_id.is_empty() {
                result = "provider error".into();
                stopped = Some("A computer call came without a call_id".into());
                break 'steps;
            }
            let actions = actions_of(call);
            let checks = safety_checks(call);
            if !checks.is_empty() && !acknowledge {
                result = "stopped at a safety check".into();
                stopped = Some(format!(
                    "The model asked to acknowledge {} safety check(s) ({}); rerun with --acknowledge-safety-checks only on your own sandboxed app",
                    checks.len(),
                    checks
                        .iter()
                        .map(|c| c["code"].as_str().unwrap_or("?").to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let record = json!({"step":step,"call_id":call_id,"actions":actions,
                    "safety_checks":checks,"acknowledged":[],"usage":entry});
                writeln!(log, "{record}")?;
                records.push(record);
                break 'steps;
            }
            for check in &checks {
                eprintln!(
                    "graphicle: step {step}: acknowledged the safety check {} ({}) as --acknowledge-safety-checks asks",
                    check["code"].as_str().unwrap_or("?"),
                    crate::fix::printable(check["message"].as_str().unwrap_or(""))
                );
                acknowledged_all.push(json!({"step":step,"call_id":call_id,"check":check}));
            }
            let shot = match screen
                .ask(&json!({"op":"act","actions":actions}))
                .and_then(shot_of)
            {
                Ok(shot) => shot,
                Err(error) => {
                    result = "driver error".into();
                    stopped = Some(error.message);
                    let record = json!({"step":step,"call_id":call_id,"actions":actions,
                        "safety_checks":checks,"usage":entry,"driver":{"error":stopped}});
                    writeln!(log, "{record}")?;
                    records.push(record);
                    break 'steps;
                }
            };
            if (shot.image.width, shot.image.height) != size {
                eprintln!(
                    "graphicle: step {step}: the screenshot's size changed from {}x{} to {}x{}",
                    size.0, size.1, shot.image.width, shot.image.height
                );
            }
            let file = format!("step-{step:02}.png");
            run.save(&file, &shot.bytes)?;
            if run.human {
                eprintln!(
                    "graphicle: step {step}: {} ({})",
                    actions.iter().map(describe).collect::<Vec<_>>().join(", "),
                    crate::spend::line(&entry)
                );
            }
            let record = json!({"step":step,"call_id":call_id,"actions":actions,
                "safety_checks":checks,"acknowledged":if checks.is_empty() {json!([])} else {json!(checks)},
                "driver":shot.reply,"screenshot":file,"usage":entry});
            writeln!(log, "{record}")?;
            records.push(record);
            if index == 0 {
                input.extend(turn.reasoning.iter().cloned());
            }
            input.push(call.clone());
            input.push(call_output(&call_id, &checks, &shot.data_url()));
            last = (file, shot.image);
        }
    }
    if result == "step cap reached" && stopped.is_none() {
        stopped = Some(format!(
            "The model was still acting after {steps} calls (--steps)"
        ));
    }
    let (text, verdict) = answer.unwrap_or_default();
    let point = verdict
        .as_ref()
        .and_then(|v| v.point)
        .map(|(x, y)| json!({"x":x,"y":y}));
    let cost: f64 = usage.iter().filter_map(|u| u["cost_usd"].as_f64()).sum();
    let tokens = |field: &str| -> u64 { usage.iter().filter_map(|u| u[field].as_u64()).sum() };
    Ok(json!({
        "result": result,
        "look_for": look_for.as_str(),
        "shown": verdict.as_ref().map(|v| v.shown.clone()),
        "point": point,
        "summary": verdict.as_ref().map(|v| v.summary.clone()).unwrap_or_default(),
        "answer": text.chars().take(ANSWER_CHARS).collect::<String>(),
        "stopped": stopped,
        "task": task,
        "model": run.profile.model,
        "profile": run.name,
        "calls": run.calls,
        "cost_usd": cost,
        "max_cost_usd": run.max_cost,
        "input_tokens": tokens("input_tokens"),
        "cached_tokens": tokens("cached_tokens"),
        "output_tokens": tokens("output_tokens"),
        "first_screenshot": "step-00.png",
        "last_screenshot": last.0,
        "last_screenshot_path": run.out.join(&last.0),
        "screen": {"width": size.0, "height": size.1},
        "acknowledged": acknowledged_all,
        "steps": records,
        "out": run.out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn profile() -> Profile {
        serde_json::from_value(
            json!({"protocol":"responses","base_url":"https://api.meta.ai/v1",
            "model":"muse-wonder-1.0","key_env":"MODEL_API_KEY",
            "price":{"input":4.0,"cached_input":0.2,"output":20.0}}),
        )
        .unwrap()
    }

    #[test]
    fn the_request_is_the_documented_shape_and_replays_calls_unchanged() {
        let call = json!({"type":"computer_call","id":"cc_1","call_id":"call_1",
            "actions":[{"type":"click","button":"left","x":4,"y":5}],
            "pending_safety_checks":[],"status":"completed","meta_safety_replay_receipt":"r1"});
        let input = vec![
            first_message("Reproduce it.", "data:image/png;base64,AAA"),
            call.clone(),
            call_output("call_1", &[], "data:image/png;base64,BBB"),
        ];
        let body = body(
            &profile(),
            INSTRUCTIONS,
            &input,
            8192,
            Some("graphicle-see:x"),
        );
        assert_eq!(body["model"], "muse-wonder-1.0");
        assert_eq!(body["tools"], json!([{"type":"computer"}]));
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["max_output_tokens"], 8192);
        assert!(body.get("truncation").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(
            body["input"][1], call,
            "a call goes back exactly as returned"
        );
        assert_eq!(
            body["input"][2],
            json!({"type":"computer_call_output","call_id":"call_1",
                "acknowledged_safety_checks":[],
                "output":{"type":"computer_screenshot","image_url":"data:image/png;base64,BBB"}})
        );
        assert_eq!(
            body["input"][0]["content"][1],
            json!({"type":"input_image","image_url":"data:image/png;base64,AAA"})
        );
        // The Meta Model API takes the prompt cache key; another provider's
        // profile does not send it.
        assert_eq!(body["prompt_cache_key"], "graphicle-see:x");
        let mut other = profile();
        other.base_url = "https://example.com/v1".into();
        let plain = super::body(&other, INSTRUCTIONS, &input, 8192, Some("k"));
        assert!(plain.get("prompt_cache_key").is_none());
    }

    #[test]
    fn replies_give_calls_messages_status_and_usage() {
        let reply = json!({"model":"muse-wonder-1.0","status":"completed","output":[
            {"type":"reasoning","id":"rs_1"},
            {"type":"computer_call","id":"cc_1","call_id":"call_1",
             "actions":[{"type":"type","text":"hello"},{"type":"keypress","keys":["CTRL","A"]}],
             "pending_safety_checks":[{"id":"csc_1","code":"malicious_instructions","message":"Careful."}]}],
            "usage":{"input_tokens":3000,"output_tokens":90,
                "input_tokens_details":{"cached_tokens":1500},
                "output_tokens_details":{"reasoning_tokens":40}}});
        let turn = parse(&reply, 12, "x");
        assert_eq!(turn.calls.len(), 1);
        // A reasoning item without encrypted content cannot be replayed.
        assert!(turn.reasoning.is_empty());
        let encrypted = json!({"output":[{"type":"reasoning","id":"rs_2",
            "summary":[{"type":"summary_text","text":"The menu."}],"encrypted_content":"abc"},
            {"type":"computer_call","call_id":"c","actions":[]}]});
        assert_eq!(
            parse(&encrypted, 1, "x").reasoning,
            vec![encrypted["output"][0].clone()]
        );
        assert_eq!(actions_of(&turn.calls[0]).len(), 2);
        assert_eq!(
            safety_checks(&turn.calls[0])[0]["code"],
            "malicious_instructions"
        );
        assert_eq!(turn.completion.input_tokens, Some(3000));
        assert_eq!(turn.completion.cached_tokens, Some(1500));
        assert_eq!(turn.completion.reasoning_tokens, Some(40));
        assert_eq!(turn.completion.model, "muse-wonder-1.0");
        let done = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
            "output":[{"type":"message","content":[{"type":"output_text","text":"{\"shown\":"}]}]});
        let turn = parse(&done, 1, "muse-spark-1.3");
        assert!(turn.calls.is_empty());
        assert_eq!(turn.status, "incomplete");
        assert_eq!(turn.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(turn.completion.model, "muse-spark-1.3");
        let failed = parse(
            &json!({"status":"failed","error":{"message":"overloaded"},"output":[]}),
            1,
            "x",
        );
        assert_eq!(
            (failed.status.as_str(), failed.error.as_deref()),
            ("failed", Some("overloaded"))
        );
        // One `action` instead of a batch.
        let single = json!({"type":"computer_call","call_id":"c","action":{"type":"wait"}});
        assert_eq!(actions_of(&single), vec![json!({"type":"wait"})]);
    }

    #[test]
    fn the_answer_is_checked_against_the_last_screenshot() {
        let v = verdict(
            "Done.\n```json\n{\"shown\":\"Bug\",\"point\":{\"x\":12,\"y\":30},\"summary\":\"The menu stays open.\"}\n```",
            100,
            50,
        )
        .unwrap();
        assert_eq!(v.shown, "bug");
        assert_eq!(v.point, Some((12, 30)));
        assert_eq!(v.summary, "The menu stays open.");
        let outside = verdict(
            "{\"shown\":\"expected\",\"point\":{\"x\":120,\"y\":3}}",
            100,
            50,
        )
        .unwrap();
        assert_eq!(outside.point, None);
        assert!(verdict("{\"shown\":\"maybe\"}", 100, 50).is_none());
        assert!(verdict("no JSON here", 100, 50).is_none());
    }

    #[test]
    fn shots_are_checked_and_estimates_count_images() {
        use base64::Engine as _;
        let shot =
            shot_of(json!({"ok":true,"done":1,"error":null,"url":"http://a/","png":PNG})).unwrap();
        assert_eq!((shot.image.width, shot.image.height), (1, 1));
        assert!(shot.data_url().starts_with("data:image/png;base64,iVBOR"));
        assert!(shot.reply.get("png").is_none());
        assert_eq!(shot.reply["url"], "http://a/");
        assert!(shot_of(json!({"ok":false,"error":"no page"})).is_err());
        assert!(shot_of(json!({"ok":true,"png":"bm90IGFuIGltYWdl"})).is_err());
        let png = format!("data:image/png;base64,{PNG}");
        let input = vec![first_message("abc", &png), call_output("c", &[], &png)];
        // Sizes come from the image's header: a 1x1 PNG counts the floor, a
        // 2560x1600 one four times it, an unreadable one as 2560x1600.
        assert_eq!(
            image_tokens(&format!("data:image/png;base64,{PNG}")),
            IMAGE_TOKENS
        );
        let mut big = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        big[16..20].copy_from_slice(&2560u32.to_be_bytes());
        big[20..24].copy_from_slice(&1600u32.to_be_bytes());
        let big = base64::engine::general_purpose::STANDARD.encode(&big);
        assert_eq!(
            image_tokens(&format!("data:image/png;base64,{big}")),
            4 * IMAGE_TOKENS
        );
        assert_eq!(image_tokens("data:image/png;base64,AAAA"), 4 * IMAGE_TOKENS);
        let estimate = input_estimate("", &input);
        assert!((2 * IMAGE_TOKENS..2 * IMAGE_TOKENS + 200).contains(&estimate));
    }

    #[test]
    fn actions_read_as_words() {
        assert_eq!(
            describe(&json!({"type":"click","button":"left","x":4,"y":5})),
            "click (4, 5)"
        );
        assert_eq!(
            describe(&json!({"type":"click","button":"right","x":4,"y":5})),
            "right click (4, 5)"
        );
        assert_eq!(
            describe(&json!({"type":"keypress","keys":["CTRL","A"]})),
            "keys CTRL+A"
        );
        assert_eq!(describe(&json!({"type":"type","text":"hi"})), "type \"hi\"");
        assert_eq!(
            describe(&json!({"type":"scroll","x":1,"y":2,"scroll_x":0,"scroll_y":300})),
            "scroll (1, 2) by (0, 300)"
        );
        assert_eq!(describe(&json!({"type":"wait"})), "wait");
    }
}
