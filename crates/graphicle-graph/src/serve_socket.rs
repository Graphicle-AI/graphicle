// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Beyond the upstream port (opt-in): a resident server for the CLI's read-only graph
//! commands.
//!
//! `serve --socket PATH` keeps the graphs that `query`, `path`, `explain`,
//! `affected`, `god-nodes` and `retrieve` load resident, and answers those
//! commands for every CLI run with `GRAPHICLE_SOCKET=PATH` set, so a command no
//! longer loads the graph (or its query snapshot) itself. The server runs the
//! same command code on the same graph file, so an answer is what the command
//! prints when run locally; a graph file that changes on disk is loaded again.
//! The CLI works locally whenever the server cannot be reached, declines the
//! request (a different engine environment or `HOME`, timing output, mode
//! flags) or fails.
//!
//! Security: the socket is created with mode 0600 and a connection is served
//! only when its peer runs as the server's user. Such a peer can read every
//! graph the server's user can read, as it could by running the CLI itself.
//! Requests are served one at a time.
use crate::{
    Result,
    cli::{Args, Output},
    graph::Graph,
    serve_search::SearchIndex,
    snapshot::Variant,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Commands the server answers.
const FORWARDED: &[&str] = &[
    "query",
    "path",
    "explain",
    "affected",
    "god-nodes",
    "god_nodes",
    "retrieve",
];
/// Flags that switch process-wide modes; commands using them run locally.
const MODE_FLAGS: &[&str] = &["precise", "retrieval", "docs", "routes"];
#[cfg(unix)]
const MAX_REQUEST: u64 = 1 << 20;
#[cfg(unix)]
const MAX_RESPONSE: u64 = 64 << 20;
const PROTOCOL: u64 = 1;

/// Set in the server process: loads go through the resident cache, and the
/// server never forwards a request of its own.
static SERVING: AtomicBool = AtomicBool::new(false);

/// The engine's own environment variables that a forwarded answer may depend
/// on. An explicit list, never a `GRAPHICLE_` prefix match: other Graphicle
/// programs' settings and secrets share the prefix and must not travel in a
/// request. Not listed: the socket's own `GRAPHICLE_SOCKET`, the MCP HTTP key
/// `GRAPHICLE_MCP_API_KEY` (a secret no CLI answer depends on), and names
/// read only at build time or by tests.
const FORWARDED_ENV: &[&str] = &[
    "GRAPHICLE_ALLOW_LOCAL_PROVIDERS",
    "GRAPHICLE_AMBIGUOUS_SCALE",
    "GRAPHICLE_API_TIMEOUT",
    "GRAPHICLE_AZURE_MODEL",
    "GRAPHICLE_BACKEND",
    "GRAPHICLE_BEDROCK_MODEL",
    "GRAPHICLE_CHANGED",
    "GRAPHICLE_CHANGED_JSON",
    "GRAPHICLE_CLAUDE_CLI_MODEL",
    "GRAPHICLE_CLAUDE_CLI_PARALLEL",
    "GRAPHICLE_DEEPSEEK_MODEL",
    "GRAPHICLE_DISABLE_THINKING",
    "GRAPHICLE_DOCS",
    "GRAPHICLE_FFMPEG",
    "GRAPHICLE_FORCE",
    "GRAPHICLE_GEMINI_MODEL",
    "GRAPHICLE_GOOGLE_WORKSPACE",
    "GRAPHICLE_GOOGLE_WORKSPACE_TIMEOUT",
    "GRAPHICLE_GRAMMAR_DIR",
    "GRAPHICLE_GRAMMAR_UNVERIFIED",
    "GRAPHICLE_HOOK_NUDGE_CAP",
    "GRAPHICLE_HOOK_STRICT",
    "GRAPHICLE_HOOK_STRICT_TTL",
    "GRAPHICLE_LIVE_CONTEXT_CACHE",
    "GRAPHICLE_LLM_TEMPERATURE",
    "GRAPHICLE_MAX_AUDIO_SECONDS",
    "GRAPHICLE_MAX_CONTEXTS",
    "GRAPHICLE_MAX_GRAPH_BYTES",
    "GRAPHICLE_MAX_GRAPH_REFS",
    "GRAPHICLE_MAX_OUTPUT_TOKENS",
    "GRAPHICLE_MAX_RETRIES",
    "GRAPHICLE_MAX_RETRY_DEPTH",
    "GRAPHICLE_MAX_WORKERS",
    "GRAPHICLE_MIN_EVIDENCE",
    "GRAPHICLE_MTIME_GRANULARITY_MS",
    "GRAPHICLE_NO_AUTO_REFRESH",
    "GRAPHICLE_NO_BACKUP",
    "GRAPHICLE_NO_INCREMENTAL_CACHE",
    "GRAPHICLE_OLLAMA_KEEP_ALIVE",
    "GRAPHICLE_OLLAMA_NUM_CTX",
    "GRAPHICLE_OLLAMA_PARALLEL",
    "GRAPHICLE_OLLAMA_SEED",
    "GRAPHICLE_OLLAMA_VISION",
    "GRAPHICLE_OPENAI_MODEL",
    "GRAPHICLE_OUT",
    "GRAPHICLE_PRECISE",
    "GRAPHICLE_PRECISE_TIMING",
    "GRAPHICLE_PROFILE",
    "GRAPHICLE_QUERY_LOG",
    "GRAPHICLE_QUERY_LOG_DISABLE",
    "GRAPHICLE_QUERY_LOG_ENABLE",
    "GRAPHICLE_QUERY_LOG_RESPONSES",
    "GRAPHICLE_QUERY_SNAPSHOT",
    "GRAPHICLE_QUERY_TIMING",
    "GRAPHICLE_REBUILD_LOG",
    "GRAPHICLE_REBUILD_MEMORY_LIMIT_MB",
    "GRAPHICLE_REBUILD_TIMEOUT",
    "GRAPHICLE_RETRIEVAL",
    "GRAPHICLE_ROUTES",
    "GRAPHICLE_SCIP",
    "GRAPHICLE_SKIP_HOOK",
    "GRAPHICLE_SOCKET_DEBUG",
    "GRAPHICLE_SOCKET_TIMEOUT",
    "GRAPHICLE_TRACE",
    "GRAPHICLE_TRANSCRIBE_TIMEOUT",
    "GRAPHICLE_TRIAGE_BACKEND",
    "GRAPHICLE_TRIAGE_MODEL",
    "GRAPHICLE_VIZ_NODE_LIMIT",
    "GRAPHICLE_WATCH_TIMING",
    "GRAPHICLE_WHISPER_CACHE",
    "GRAPHICLE_WHISPER_MODEL",
    "GRAPHICLE_WHISPER_OFFLINE",
    "GRAPHICLE_WHISPER_PROMPT",
    "GRAPHICLE_WHISPER_THREADS",
    "GRAPHICLE_YT_DLP",
];

/// The environment a forwarded answer may depend on: the set variables of
/// [`FORWARDED_ENV`] and `HOME`. `None` when one of them is not UTF-8, which
/// is then left to local work.
fn relevant_env() -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for key in FORWARDED_ENV.iter().copied().chain(["HOME"]) {
        if let Some(value) = std::env::var_os(key) {
            out.insert(key.to_string(), value.into_string().ok()?);
        }
    }
    Some(out)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FileStamp {
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
    inode: u64,
    device: u64,
    changed: (i64, i64),
}
impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let (inode, device, changed) = {
            use std::os::unix::fs::MetadataExt;
            (meta.ino(), meta.dev(), (meta.ctime(), meta.ctime_nsec()))
        };
        #[cfg(not(unix))]
        let (inode, device, changed) = (0, 0, (0, 0));
        Some(FileStamp {
            path: path.to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
            inode,
            device,
            changed,
        })
    }
}
type IndexedKey = (FileStamp, u8, Option<bool>, Option<bool>, usize);
type Indexed = (Arc<Graph>, Arc<SearchIndex>);
#[derive(Default)]
struct Resident {
    indexed: BTreeMap<IndexedKey, Indexed>,
    graphs: BTreeMap<(FileStamp, usize), Arc<Graph>>,
}
/// Graphs a server keeps; entries of a file that changed are dropped when it
/// is next loaded, and at most this many graphs of each kind stay resident.
const RESIDENT_GRAPHS: usize = 8;
static RESIDENT: LazyLock<Mutex<Resident>> = LazyLock::new(Mutex::default);

fn variant_tag(variant: Variant) -> u8 {
    match variant {
        Variant::Query => 0,
        Variant::Explain => 1,
        Variant::Path => 2,
        Variant::Serve => 3,
    }
}
/// [`crate::cli::load_indexed`], served from the resident cache in a server.
pub fn shared_indexed(
    path: &Path,
    direction: Option<bool>,
    multigraph: Option<bool>,
    variant: Variant,
) -> Result<Indexed> {
    let load = || {
        crate::cli::load_indexed(path, direction, multigraph, variant)
            .map(|(g, index)| (Arc::new(g), Arc::new(index)))
    };
    if !SERVING.load(Ordering::Relaxed) {
        return load();
    }
    let Some(stamp) = FileStamp::of(path) else {
        return load();
    };
    let key = (
        stamp,
        variant_tag(variant),
        direction,
        multigraph,
        crate::extractors::evidence::min_level(),
    );
    if let Some(hit) = lock().indexed.get(&key) {
        return Ok(hit.clone());
    }
    let loaded = load()?;
    let mut resident = lock();
    resident
        .indexed
        .retain(|k, _| k.0.path != key.0.path || k.0 == key.0);
    while resident.indexed.len() >= RESIDENT_GRAPHS {
        let oldest = resident.indexed.keys().next().cloned().unwrap();
        resident.indexed.remove(&oldest);
    }
    resident.indexed.insert(key, loaded.clone());
    Ok(loaded)
}
/// [`crate::affected::load_graph`], served from the resident cache in a server.
pub fn shared_graph(path: &Path) -> Result<Arc<Graph>> {
    let load = || crate::affected::load_graph(path).map(Arc::new);
    if !SERVING.load(Ordering::Relaxed) {
        return load();
    }
    let Some(stamp) = FileStamp::of(path) else {
        return load();
    };
    let key = (stamp, crate::extractors::evidence::min_level());
    if let Some(hit) = lock().graphs.get(&key) {
        return Ok(hit.clone());
    }
    let loaded = load()?;
    let mut resident = lock();
    resident
        .graphs
        .retain(|k, _| k.0.path != key.0.path || k.0 == key.0);
    while resident.graphs.len() >= RESIDENT_GRAPHS {
        let oldest = resident.graphs.keys().next().cloned().unwrap();
        resident.graphs.remove(&oldest);
    }
    resident.graphs.insert(key, loaded.clone());
    Ok(loaded)
}
fn lock() -> std::sync::MutexGuard<'static, Resident> {
    RESIDENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// The request for `command` with parsed arguments `a`: the graph path (and
/// `retrieve`'s `--root`) made absolute, and the caller's working directory,
/// which the server enters for the request (answers show paths relative to
/// it).
fn request(command: &str, a: &Args, env: BTreeMap<String, String>) -> Option<Value> {
    let absolute = |p: PathBuf| std::path::absolute(p).ok();
    let mut values = a.values.clone();
    let graph = absolute(crate::cli_state::resolve(&a.graph()))?;
    values.insert("graph".into(), vec![graph.to_str()?.to_string()]);
    if let Some(root) = a.get("root") {
        let root = absolute(crate::cli::expand(root))?;
        values.insert("root".into(), vec![root.to_str()?.to_string()]);
    }
    Some(json!({
        "protocol": PROTOCOL,
        "command": command,
        "pos": a.pos,
        "values": values,
        "flags": a.flags,
        "min_level": crate::extractors::evidence::min_level(),
        "env": env,
        "cwd": std::env::current_dir().ok()?.to_str()?,
    }))
}

/// Answer `command` through the server named by `GRAPHICLE_SOCKET`, when one
/// accepts it: `Some` with the command's result (its output is in `out`),
/// `None` to run the command locally (nothing was written to `out`).
pub fn try_forward(command: &str, a: &Args, out: &mut Output) -> Option<Result<()>> {
    if SERVING.load(Ordering::Relaxed)
        || !FORWARDED.contains(&command)
        || MODE_FLAGS.iter().any(|f| a.flag(f))
        || crate::snapshot::timing()
    {
        return None;
    }
    let socket = std::env::var_os("GRAPHICLE_SOCKET").filter(|s| !s.is_empty())?;
    // GRAPHICLE_SOCKET_DEBUG=1 says on stderr why a command ran locally.
    let debug = |why: &str| {
        if std::env::var_os("GRAPHICLE_SOCKET_DEBUG").is_some() {
            eprintln!("[graphicle socket] {command} ran locally: {why}");
        }
    };
    let Some(request) = relevant_env().and_then(|env| request(command, a, env)) else {
        debug("request not representable");
        return None;
    };
    let Some(reply) = transport::exchange(Path::new(&socket), &request) else {
        debug("no answer from the server");
        return None;
    };
    if reply["protocol"] != PROTOCOL || reply["handled"] != true {
        debug(&format!(
            "declined ({})",
            reply["reason"].as_str().unwrap_or("protocol")
        ));
        return None;
    }
    // Validate the whole reply before publishing any output: fallback must
    // never append a local answer to half of a malformed server answer.
    let stdout = reply["stdout"].as_str()?;
    let stderr = reply["stderr"].as_str()?;
    let exit_code = i32::try_from(reply["exit_code"].as_i64()?).ok()?;
    if !reply["error"].is_null() && !reply["error"].is_string() {
        return None;
    }
    out.stdout.push_str(stdout);
    out.stderr.push_str(stderr);
    out.exit_code = exit_code;
    Some(match reply["error"].as_str() {
        Some(error) => Err(error.to_string().into()),
        None => Ok(()),
    })
}

/// Serve one request (see [`request`]); `handled: false` asks the caller to
/// work locally.
fn answer(request: &Value) -> Value {
    let decline = |reason: &str| json!({"protocol": PROTOCOL, "handled": false, "reason": reason});
    if request["protocol"] != PROTOCOL {
        return decline("protocol");
    }
    let Some(command) = request["command"]
        .as_str()
        .filter(|c| FORWARDED.contains(c))
    else {
        return decline("command");
    };
    let env: Option<BTreeMap<String, String>> = serde_json::from_value(request["env"].clone()).ok();
    if env.is_none() || env != relevant_env() {
        return decline("environment");
    }
    let parsed = (|| -> Option<Args> {
        Some(Args {
            pos: serde_json::from_value(request["pos"].clone()).ok()?,
            values: serde_json::from_value(request["values"].clone()).ok()?,
            flags: serde_json::from_value::<BTreeSet<String>>(request["flags"].clone()).ok()?,
        })
    })();
    let (Some(a), Some(level)) = (parsed, request["min_level"].as_u64()) else {
        return decline("request");
    };
    if MODE_FLAGS.iter().any(|f| a.flag(f)) {
        return decline("mode");
    }
    // Requests are served one at a time, so the process's working directory
    // and evidence level can be the caller's for the request's duration.
    let (Some(cwd), Ok(home)) = (request["cwd"].as_str(), std::env::current_dir()) else {
        return decline("request");
    };
    if std::env::set_current_dir(cwd).is_err() {
        return decline("cwd");
    }
    let level = usize::try_from(level).unwrap_or(usize::MAX);
    let evidence = &crate::extractors::evidence::MIN_LEVEL;
    let previous = evidence.swap(level, Ordering::Relaxed);
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = Output::default();
        let result = if command == "retrieve" {
            crate::cli_commands::retrieve(&a, &mut out)
        } else {
            crate::cli_commands::query(command, &a, &mut out)
        };
        (out, result)
    }));
    evidence.store(previous, Ordering::Relaxed);
    let _ = std::env::set_current_dir(home);
    let Ok((out, result)) = run else {
        return decline("panic");
    };
    json!({
        "protocol": PROTOCOL,
        "handled": true,
        "stdout": out.stdout,
        "stderr": out.stderr,
        "exit_code": out.exit_code,
        "error": result.err().map(|e| e.to_string()),
    })
}

/// `serve --socket PATH`: answer forwarded commands until the process ends.
pub fn serve(path: &Path) -> Result<()> {
    SERVING.store(true, Ordering::Relaxed);
    let result = transport::listen(path, answer);
    SERVING.store(false, Ordering::Relaxed);
    result
}

#[cfg(unix)]
mod transport {
    use super::{MAX_REQUEST, MAX_RESPONSE};
    use crate::Result;
    use serde_json::Value;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        os::unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::Path,
        time::Duration,
    };

    /// Whether the connected peer runs as this process's user.
    fn same_user(stream: &UnixStream) -> bool {
        use std::os::fd::AsRawFd;
        let fd = stream.as_raw_fd();
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: `fd` is an open socket and both out-pointers are valid.
            let ok = unsafe { nix::libc::getpeereid(fd, &mut uid, &mut gid) } == 0;
            ok && uid == nix::unistd::getuid().as_raw()
        }
        #[cfg(target_os = "linux")]
        {
            let mut cred = nix::libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut len = std::mem::size_of::<nix::libc::ucred>() as nix::libc::socklen_t;
            // SAFETY: `cred` and `len` describe a buffer of the option's size.
            let ok = unsafe {
                nix::libc::getsockopt(
                    fd,
                    nix::libc::SOL_SOCKET,
                    nix::libc::SO_PEERCRED,
                    (&mut cred as *mut nix::libc::ucred).cast(),
                    &mut len,
                )
            } == 0;
            ok && cred.uid == nix::unistd::getuid().as_raw()
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "linux"
        )))]
        {
            let _ = fd;
            false
        }
    }
    fn read_line(stream: impl Read, limit: u64) -> Option<Value> {
        let mut line = Vec::new();
        BufReader::new(stream.take(limit))
            .read_until(b'\n', &mut line)
            .ok()?;
        if line.last() != Some(&b'\n') {
            return None;
        }
        serde_json::from_slice(&line).ok()
    }
    fn write_line(mut stream: impl Write, value: &Value, limit: u64) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        if bytes.len() as u64 >= limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "socket message exceeds limit",
            ));
        }
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        stream.flush()
    }
    /// One request and its reply; `None` on any failure (the caller then
    /// works locally).
    pub fn exchange(path: &Path, request: &Value) -> Option<Value> {
        let stream = UnixStream::connect(path).ok()?;
        let timeout = std::env::var("GRAPHICLE_SOCKET_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(120);
        stream
            .set_read_timeout(Some(Duration::from_secs(timeout.max(1))))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .ok()?;
        write_line(&stream, request, MAX_REQUEST).ok()?;
        read_line(&stream, MAX_RESPONSE)
    }
    pub fn listen(path: &Path, answer: impl Fn(&Value) -> Value) -> Result<()> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(format!("{} exists and is not a socket", path.display()).into());
            }
            if UnixStream::connect(path).is_ok() {
                return Err(format!("a server is already listening on {}", path.display()).into());
            }
            std::fs::remove_file(path).map_err(crate::Error::from)?;
        }
        let listener = UnixListener::bind(path).map_err(crate::Error::from)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(crate::Error::from)?;
        eprintln!(
            "[graphicle serve] answering CLI queries on {}",
            path.display()
        );
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if !same_user(&stream) {
                continue;
            }
            if stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .is_err()
                || stream
                    .set_write_timeout(Some(Duration::from_secs(30)))
                    .is_err()
            {
                continue;
            }
            let Some(request) = read_line(&stream, MAX_REQUEST) else {
                continue;
            };
            let reply = answer(&request);
            let _ = write_line(&stream, &reply, MAX_RESPONSE);
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod transport {
    use crate::Result;
    use serde_json::Value;
    use std::path::Path;
    pub fn exchange(_path: &Path, _request: &Value) -> Option<Value> {
        None
    }
    pub fn listen(_path: &Path, _answer: impl Fn(&Value) -> Value) -> Result<()> {
        Err("serve --socket needs Unix domain sockets".into())
    }
}

#[cfg(test)]
mod tests {
    /// Every engine variable the sources read (a quoted `"GRAPHICLE_…"` name,
    /// or `${GRAPHICLE_…` in a hook script) is forwarded or deliberately not.
    #[test]
    fn every_engine_variable_is_forwarded_or_deliberately_left_out() {
        const LEFT_OUT: &[&str] = &[
            "GRAPHICLE_SOCKET",
            "GRAPHICLE_MCP_API_KEY",
            "GRAPHICLE_SNAPSHOT_CODE",
            "GRAPHICLE_PACK_DIGESTS",
            "GRAPHICLE_SET_ORDER_CASES",
            "GRAPHICLE_STR_SET_CASES",
            "GRAPHICLE_VISIT_CORPUS",
        ];
        let read = regex::Regex::new(r#"(?:"|\$\{)(GRAPHICLE_[A-Z0-9_]*[A-Z0-9])"#).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(text) = std::fs::read_to_string(&path) {
                    seen.extend(read.captures_iter(&text).map(|c| c[1].to_string()));
                }
            }
        }
        assert!(seen.contains("GRAPHICLE_OUT") && seen.contains("GRAPHICLE_SKIP_HOOK"));
        for key in &seen {
            let key = key.as_str();
            assert!(
                super::FORWARDED_ENV.contains(&key) != LEFT_OUT.contains(&key),
                "{key}: list it in exactly one of FORWARDED_ENV and LEFT_OUT"
            );
        }
        assert!(super::FORWARDED_ENV.windows(2).all(|w| w[0] < w[1]));
    }
}
