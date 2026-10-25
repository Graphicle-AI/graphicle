// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/paths.py, graphify/security.py
//! Port of graphify/security.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
//! Transport uses native Rust HTTP/TLS with a validated, pinned DNS address.
use crate::{
    Result, fsutil, ip_policy_data,
    python_compat::{repr_string, string},
};
use reqwest::Url;
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

pub const MAX_FETCH_BYTES: usize = 52_428_800;
pub const MAX_TEXT_BYTES: usize = 10_485_760;
pub const MAX_GRAPH_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_LABEL_LEN: usize = 256;
pub const METADATA_MAX_VALUE_LEN: usize = 512;
pub const METADATA_MAX_LIST_ITEMS: usize = 50;
pub const METADATA_MAX_ATTRIBUTES: usize = 100; // Upstream defines but does not enforce it.
/// The output directory when `GRAPHICLE_OUT` is not set.
pub const DEFAULT_OUT_DIR: &str = ".graphicle";
/// The output directory: `GRAPHICLE_OUT` (a name relative to the project root
/// or an absolute path), else [`DEFAULT_OUT_DIR`] (see [`out_dir_from`]).
pub static OUT_DIR: LazyLock<String> = LazyLock::new(|| {
    let raw = std::env::var("GRAPHICLE_OUT").ok();
    let (dir, warning) = out_dir_from(raw.as_deref());
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    dir
});
/// `_out_from_env` (Graphify 0.9.86, #4329): `GRAPHICLE_OUT` stripped, or the
/// default when it is unset or blank, or names the project itself or a parent
/// of it (`.`, `..`, `../..`): output there would land in the project root and
/// `uninstall --purge` would delete the project. The second value is the
/// warning a rejected value earns.
pub fn out_dir_from(raw: Option<&str>) -> (String, Option<String>) {
    let whitespace = |c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c);
    let value = raw.unwrap_or("").trim_matches(whitespace);
    if value.is_empty() {
        return (DEFAULT_OUT_DIR.into(), None);
    }
    let normalized = crate::paths::normpath(value);
    let only_parents = std::path::Path::new(&normalized)
        .components()
        .all(|c| matches!(c, std::path::Component::ParentDir));
    if normalized == "." || only_parents {
        let warning = format!(
            "warning: GRAPHICLE_OUT={} points at the project or a parent of it; using {} instead",
            crate::python_compat::repr_string(value),
            crate::python_compat::repr_string(DEFAULT_OUT_DIR)
        );
        return (DEFAULT_OUT_DIR.into(), Some(warning));
    }
    (value.into(), None)
}

pub fn out_dir_name(output: &str) -> String {
    crate::paths::out_dir_name(output)
}

pub fn max_graph_file_bytes_from(raw: &str, default: u64) -> u64 {
    let whitespace = |c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c);
    let raw = raw.trim_matches(whitespace).to_uppercase();
    let (number, multiplier) = if let Some(value) = raw.strip_suffix("GB") {
        (value.trim_matches(whitespace), 1024u64 * 1024 * 1024)
    } else if let Some(value) = raw.strip_suffix("MB") {
        (value.trim_matches(whitespace), 1024u64 * 1024)
    } else {
        (raw.as_str(), 1)
    };
    // int() accepts single internal underscores, but not a leading/trailing one.
    let digits = number.strip_prefix('+').unwrap_or(number);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return default;
    }
    let mut decimal = String::new();
    for c in digits.chars() {
        if c == '_' {
            decimal.push(c);
            continue;
        }
        let cp = c as u32;
        let index = crate::decimal_data::DECIMAL.partition_point(|(start, _)| *start <= cp);
        if index == 0 || cp > crate::decimal_data::DECIMAL[index - 1].1 {
            return default;
        }
        decimal
            .push((b'0' + ((cp - crate::decimal_data::DECIMAL[index - 1].0) % 10) as u8) as char);
    }
    let parsed = decimal
        .replace('_', "")
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0);
    parsed
        .and_then(|n| n.checked_mul(multiplier))
        .unwrap_or(default)
}

pub fn max_graph_file_bytes() -> u64 {
    max_graph_file_bytes_from(
        &std::env::var("GRAPHICLE_MAX_GRAPH_BYTES").unwrap_or_default(),
        MAX_GRAPH_FILE_BYTES,
    )
}

pub fn ip_is_blocked(ip: IpAddr) -> bool {
    let (value, ranges) = match ip {
        IpAddr::V4(ip) => (u32::from(ip) as u128, ip_policy_data::BLOCKED_V4),
        IpAddr::V6(ip) => {
            let value = u128::from(ip);
            if value >> 32 == 0x0064_ff9b_0000_0000_0000_0000 {
                (u128::from(value as u32), ip_policy_data::BLOCKED_V4)
            } else {
                (value, ip_policy_data::BLOCKED_V6)
            }
        }
    };
    let index = ranges.partition_point(|(start, _)| *start <= value);
    index > 0 && value <= ranges[index - 1].1
}

pub trait Resolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>>;
}

pub struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        (host.trim_matches(['[', ']']), port)
            .to_socket_addrs()
            .map(Iterator::collect)
            .map_err(|error| format!("OSError: {error}"))
            .map_err(crate::Error::from)
    }
}

fn parsed_url(url: &str) -> Result<Url> {
    let (scheme, rest) = url
        .split_once(':')
        .map(|(s, rest)| (s.to_ascii_lowercase(), rest))
        .unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "ValueError: Blocked URL scheme '{scheme}' - only http and https are allowed. Got: {}",
            repr_string(url)
        )
        .into());
    }
    check_netloc(url, rest)?;
    Ok(Url::parse(url)
        .map_err(|error| format!("ValueError: Invalid URL {}: {error}", repr_string(url)))?)
}
/// Graphify 0.9.94 (#4643, #4644, #4645): embedded `user:pass@` credentials,
/// a non-numeric, out-of-range or zero port, and a missing hostname are
/// refused before any request, and again on every redirect. The netloc is
/// split the way Python's `urllib.parse.urlparse` splits it: the text after
/// `//` up to the first `/`, `?` or `#`, with tabs and line breaks removed.
fn check_netloc(url: &str, rest: &str) -> Result<()> {
    let refuse = |what: &str| -> crate::Error {
        format!("ValueError: Blocked URL {what}. Got: {}", repr_string(url)).into()
    };
    let rest = rest.replace(['\t', '\r', '\n'], "");
    let netloc = match rest.strip_prefix("//") {
        Some(after) => &after[..after.find(['/', '?', '#']).unwrap_or(after.len())],
        None => "",
    };
    // Any `@` gives Python a (possibly empty) username.
    if netloc.contains('@') {
        return Err(refuse("containing embedded credentials"));
    }
    let (host, port) = match netloc.split_once('[') {
        Some((_, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, after.split_once(':').map_or("", |(_, port)| port))
        }
        None => netloc.split_once(':').unwrap_or((netloc, "")),
    };
    if !port.is_empty() {
        let number = port
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| port.parse::<u32>().ok())
            .flatten()
            .filter(|n| *n <= 65535);
        match number {
            None => return Err(refuse("with an invalid port")),
            Some(0) => return Err(refuse("with port 0")),
            Some(_) => {}
        }
    }
    if host.is_empty() {
        return Err(refuse("without a hostname"));
    }
    Ok(())
}

/// A redirect's URL as Python's `urljoin(current, location)` writes it, for
/// [`check_netloc`]: a Location with another scheme, or with a netloc of its
/// own, as given (a scheme-relative one under the current scheme); otherwise
/// `joined`, which keeps the current, already checked netloc. As `urlsplit`,
/// leading control characters and spaces, and tabs and line breaks, are
/// dropped first.
fn urljoin_text(current: &Url, location: &str, joined: &Url) -> String {
    let cleaned: String = location
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    // Some(true): another scheme, or this one with a netloc of its own.
    let as_written = cleaned
        .split_once(':')
        .filter(|(scheme, _)| {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        })
        .map(|(scheme, rest)| {
            !scheme.eq_ignore_ascii_case(current.scheme()) || rest.starts_with("//")
        });
    match as_written {
        Some(true) => cleaned,
        None if cleaned.starts_with("//") => format!("{}:{cleaned}", current.scheme()),
        _ => joined.as_str().to_owned(),
    }
}

pub fn validate_url_with(url: &str, resolver: &impl Resolver) -> Result<String> {
    let parsed = parsed_url(url)?;
    if let Some(host) = parsed.host_str() {
        if matches!(host, "metadata.google.internal" | "metadata.google.com") {
            return Err(format!(
                "ValueError: Blocked cloud metadata endpoint '{host}'. Got: {}",
                repr_string(url)
            )
            .into());
        }
        let addresses = resolver
            .resolve(host, parsed.port_or_known_default().unwrap_or(80))
            .map_err(|error| {
                format!(
                    "ValueError: DNS resolution failed for '{host}': {error}. Got: {}",
                    repr_string(url)
                )
            })?;
        for address in addresses {
            if ip_is_blocked(address.ip()) {
                return Err(format!(
                    "ValueError: Blocked private/internal IP {} (resolved from '{host}'). Got: {}",
                    address.ip(),
                    repr_string(url)
                )
                .into());
            }
        }
    }
    Ok(url.to_owned())
}

pub fn validate_url(url: &str) -> Result<String> {
    validate_url_with(url, &SystemResolver)
}

pub fn resolve_and_validate(host: &str, port: u16, resolver: &impl Resolver) -> Result<SocketAddr> {
    let addresses = resolver.resolve(host, port)?;
    if let Some(address) = addresses.into_iter().next() {
        if ip_is_blocked(address.ip()) {
            return Err(format!(
                "OSError: SSRF blocked: IP {} resolved from '{host}' is private/reserved",
                address.ip()
            )
            .into());
        }
        return Ok(address);
    }
    Err(format!("OSError: SSRF blocked: no usable address resolved from '{host}'").into())
}

pub struct FetchResponse {
    pub status: u16,
    pub location: Option<String>,
    pub body: Box<dyn Read + Send>,
}

pub trait Transport {
    fn get(&mut self, url: &Url, address: SocketAddr, timeout: Duration) -> Result<FetchResponse>;
}

pub struct PinnedHttpTransport;

fn http_error(error: reqwest::Error) -> String {
    use std::error::Error;
    let mut details = vec![error.to_string()];
    let mut cause = error.source();
    while let Some(error) = cause {
        details.push(error.to_string());
        cause = error.source();
    }
    crate::net::with_hint(format!("URLError: {}", details.join(": ")))
}
impl Transport for PinnedHttpTransport {
    fn get(&mut self, url: &Url, address: SocketAddr, timeout: Duration) -> Result<FetchResponse> {
        let host = url.host_str().ok_or("ValueError: URL has no hostname")?;
        let client = crate::net::blocking_client(|builder| {
            builder
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .resolve_to_addrs(host, &[address])
                .user_agent(concat!("Mozilla/5.0 graphicle/", env!("CARGO_PKG_VERSION")))
        })
        .map_err(http_error)?;
        // The URL keeps the original hostname for Host, SNI and certificate checks.
        let response = client.get(url.clone()).send().map_err(http_error)?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(FetchResponse {
            status,
            location,
            body: Box::new(response),
        })
    }
}

pub fn safe_fetch_with(
    url: &str,
    max_bytes: usize,
    timeout: Duration,
    resolver: &impl Resolver,
    transport: &mut impl Transport,
) -> Result<Vec<u8>> {
    validate_url_with(url, resolver)?;
    let mut current = parsed_url(url)?;
    let mut redirects = HashMap::new();
    let mut redirect_count = 0;
    loop {
        let host = current
            .host_str()
            .ok_or("ValueError: URL has no hostname")?;
        let address = resolve_and_validate(
            host,
            current.port_or_known_default().unwrap_or(80),
            resolver,
        )?;
        let mut response = transport.get(&current, address, timeout)?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308)
            && let Some(location) = response.location
        {
            let target = current
                .join(&location)
                .map_err(|error| format!("ValueError: Invalid redirect: {error}"))?;
            // Checked first as upstream checks it: urllib hands
            // `redirect_request` the URL as `urljoin` writes it, with an
            // absolute or scheme-relative Location's netloc as given, so
            // `https://@example.com/` is refused (an empty username) instead
            // of being normalised to `https://example.com/` and followed.
            validate_url_with(&urljoin_text(&current, &location, &target), resolver)?;
            validate_url_with(target.as_str(), resolver)?;
            let count = redirects.entry(target.as_str().to_owned()).or_insert(0);
            *count += 1;
            redirect_count += 1;
            if *count > 4 || redirect_count > 10 {
                return Err(format!("HTTPError: {}: redirect loop", response.status).into());
            }
            current = target;
            continue;
        }
        if !(200..300).contains(&response.status) {
            return Err(format!("HTTPError: {}: HTTP {}", response.status, response.status).into());
        }
        let mut content = Vec::new();
        let mut buffer = [0u8; 65_536];
        loop {
            let count = response
                .body
                .read(&mut buffer)
                .map_err(|error| format!("OSError: {error}"))?;
            if count == 0 {
                break;
            }
            if count > max_bytes.saturating_sub(content.len()) {
                return Err(format!(
                    "OSError: Response from {} exceeds size limit ({} MB). Aborting download.",
                    repr_string(url),
                    max_bytes / 1_048_576
                )
                .into());
            }
            content.extend_from_slice(&buffer[..count]);
        }
        return Ok(content);
    }
}

pub fn safe_fetch(url: &str, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>> {
    safe_fetch_with(
        url,
        max_bytes,
        timeout,
        &SystemResolver,
        &mut PinnedHttpTransport,
    )
}
pub fn safe_fetch_text(url: &str, max_bytes: usize, timeout: Duration) -> Result<String> {
    Ok(String::from_utf8_lossy(&safe_fetch(url, max_bytes, timeout)?).into_owned())
}

pub fn validate_graph_path_with(
    path: &Path,
    base: Option<&Path>,
    output: &str,
    output_name: &str,
) -> Result<PathBuf> {
    let resolved = fsutil::resolve_missing(path).map_err(|error| format!("OSError: {error}"))?;
    let base = if let Some(base) = base {
        base.to_path_buf()
    } else {
        resolved
            .ancestors()
            .find(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|name| name == output_name)
            })
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(output))
    };
    let base = fsutil::resolve_missing(&base).map_err(|error| format!("OSError: {error}"))?;
    if !base.exists() {
        return Err(format!(
            "ValueError: Graph base directory does not exist: {}. Run /graphicle first to build the graph.",
            base.display()
        ).into());
    }
    if !resolved.starts_with(&base) {
        return Err(format!(
            "ValueError: Path {} escapes the allowed directory {}. Only paths inside .graphicle/ are permitted.",
            repr_string(&fsutil::path_string(path)),
            base.display()
        ).into());
    }
    if !resolved.exists() {
        return Err(format!(
            "FileNotFoundError: Graph file not found: {}",
            resolved.display()
        )
        .into());
    }
    Ok(resolved)
}
pub fn validate_graph_path(path: &Path, base: Option<&Path>) -> Result<PathBuf> {
    validate_graph_path_with(path, base, &OUT_DIR, &out_dir_name(&OUT_DIR))
}

fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

pub fn check_graph_file_size_cap_with(path: &Path, cap: u64) -> Result<()> {
    if let Ok(metadata) = path.metadata()
        && metadata.len() > cap
    {
        return Err(format!(
            "ValueError: graph file {} is {} bytes, exceeds {}-byte cap\n(set GRAPHICLE_MAX_GRAPH_BYTES=<bytes> or GRAPHICLE_MAX_GRAPH_BYTES=<N>GB to raise the limit)",
            fsutil::path_string(path),
            grouped(metadata.len()),
            grouped(cap)
        ).into());
    }
    Ok(())
}
pub fn check_graph_file_size_cap(path: &Path) -> Result<()> {
    check_graph_file_size_cap_with(path, max_graph_file_bytes())
}

/// The size cap where the engine reads a graph to update, serve or query it:
/// [`check_graph_file_size_cap`], except that a graph.json this engine wrote
/// itself ([`note_written_graph`]) is read whatever its size, unless
/// `GRAPHICLE_MAX_GRAPH_BYTES` is set. The cap guards against a graph file
/// from elsewhere (one a repository ships, or another machine wrote) that
/// would exhaust memory; the engine's own output of a large project, such as
/// kubernetes (over 512 MiB), is not one, and refusing it stopped every
/// `update` after the first. Graphicle's own rule; upstream caps every read.
///
/// A large graph.json without a record stays refused, whoever wrote it: one
/// an earlier release wrote (before the records) is refused like one from
/// elsewhere, and the error says how to rebuild it once so that it gets one.
pub fn check_own_graph_size_cap(path: &Path) -> Result<()> {
    let Err(error) = check_graph_file_size_cap(path) else {
        return Ok(());
    };
    if std::env::var("GRAPHICLE_MAX_GRAPH_BYTES").is_ok() {
        return Err(error);
    }
    if graph_stamp(path).is_some_and(|(stamp, text)| {
        std::fs::read_to_string(stamp).is_ok_and(|saved| saved == text)
    }) {
        return Ok(());
    }
    Err(format!(
        "{error}\n(this engine reads a graph.json it wrote itself at any size, but has no record \
         of writing this one, as for one an earlier release wrote: if it is the project's own, \
         rebuild it once with `GRAPHICLE_MAX_GRAPH_BYTES=<N>GB {} extract --force <project>` \
         (add --code-only without an LLM backend); the graph.json that writes is read without \
         the setting)",
        crate::cli::program()
    )
    .into())
}

/// The folder of the per-user records of large graph files this engine wrote:
/// `~/.graphicle-graph/graph-stamps`, outside every project, so a repository
/// cannot ship one.
fn graph_stamp_folder() -> Option<PathBuf> {
    crate::global_graph::GlobalStore::default_location()
        .ok()
        .map(|store| store.directory.join("graph-stamps"))
}

/// The record of the graph file at `path` as it is now: its record file
/// (named by a digest of the resolved path) and the text it holds, the path,
/// size, modification time and, on Unix, device and inode. Any later change
/// of the file makes the text differ.
fn graph_stamp(path: &Path) -> Option<(PathBuf, String)> {
    use sha2::Digest;
    let real = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&real).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let file = String::new();
    let name = fsutil::path_string(&real);
    let key = format!("{:x}", sha2::Sha256::digest(name.as_bytes()));
    let text = serde_json::json!({"path": name, "bytes": metadata.len(),
        "modified_ns": modified.to_string(), "file": file})
    .to_string();
    Some((
        graph_stamp_folder()?.join(format!("{}.json", &key[..32])),
        text,
    ))
}

/// Called after every rename into place (`paths::os_replace_with_fallback_with`,
/// which atomic writes, `paths::replace_file` and the rebuild's temporary
/// graph go through): a graph.json larger than the default cap gets a record
/// for [`check_own_graph_size_cap`]; a smaller one drops a record its path
/// had. Failures are ignored: without a record the cap applies, as before.
pub fn note_written_graph(path: &Path) {
    if path.file_name().and_then(|name| name.to_str()) != Some("graph.json") {
        return;
    }
    let Some((stamp, text)) = graph_stamp(path) else {
        return;
    };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_GRAPH_FILE_BYTES) {
        if let Some(folder) = stamp.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        let _ = std::fs::write(&stamp, text);
    } else {
        let _ = std::fs::remove_file(&stamp);
    }
}

fn no_controls(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().filter(|c| *c > '\x1f' && *c != '\x7f')
}
/// `_SECRET_SHAPE_RE` (Graphify 0.9.93, #4566): credential shapes redacted from
/// free text copied into labels: PEM private-key headers, vendor-prefixed
/// tokens, JWTs, `Bearer` tokens and `user:pass@` URL credentials. Shape-based
/// only, so risk reduction rather than a guarantee. Python's `\b` is spelled
/// with its own word class (`regex_compat::word_class`), next to an ASCII
/// literal; `\s` is Python's whitespace (`fsutil::PY_WHITESPACE`).
static SECRET_SHAPE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    let w = crate::regex_compat::word_class();
    let space = crate::fsutil::PY_WHITESPACE
        .trim_start_matches('[')
        .trim_end_matches(']');
    let boundary = format!("(?:(?<={w})(?!{w})|(?<!{w})(?={w}))");
    let pattern = [
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----".to_owned(),
        format!("(?<!{w})(?:AKIA|ASIA)[0-9A-Z]{{16}}(?!{w})"),
        format!("(?<!{w})gh[pousr]_[A-Za-z0-9]{{36,}}"),
        format!("(?<!{w})github_pat_[A-Za-z0-9_]{{22,}}"),
        format!("(?<!{w})xox[baprs]-[A-Za-z0-9-]{{10,}}"),
        format!("(?<!{w})[sr]k_(?:live|test)_[A-Za-z0-9]{{10,}}"),
        format!("(?<!{w})sk-(?:ant-|proj-)?[A-Za-z0-9_-]{{20,}}"),
        format!("(?<!{w})AIza[0-9A-Za-z_-]{{35}}{boundary}"),
        format!("(?<!{w})eyJ[A-Za-z0-9_-]{{10,}}\\.[A-Za-z0-9_-]{{10,}}\\.[A-Za-z0-9_-]{{10,}}"),
        // `(?<=\bBearer )`: the seven characters before are `Bearer ` and the
        // one before those is no word character.
        format!("(?<=Bearer )(?<!{w}.{{7}})[A-Za-z0-9._~+/=-]{{16,}}"),
        format!("(?<=://)[^{space}/:@]+:[^{space}/@]+(?=@)"),
    ]
    .join("|");
    fancy_regex::Regex::new(&pattern).expect("verified credential pattern")
});
pub const REDACTED_MARKER: &str = "[redacted]";
/// `redact_secrets`: credential-shaped substrings of `text` replaced with
/// `[redacted]`, applied to docstring and comment text before it becomes a
/// rationale label.
///
/// The text is searched one line at a time, which gives upstream's
/// whole-text `re.sub` result: no shape matches across a line break (its
/// classes and `.` exclude `\n`, and its lookarounds see a line's edge as
/// they see the `\n` beside it). The engine's backtracking limit then applies
/// to each line rather than to the whole text, which a long docstring of
/// ordinary prose exhausted (from about 50,000 characters), so that it came
/// back as `[redacted]` whole. A line the engine cannot finish is still
/// redacted whole rather than keep a possible secret: a known difference for
/// a single line of tens of thousands of characters.
pub fn redact_secrets(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        match SECRET_SHAPE.try_replacen(body, 0, fancy_regex::NoExpand(REDACTED_MARKER)) {
            Ok(redacted) => out.push_str(&redacted),
            Err(_) => out.push_str(REDACTED_MARKER),
        }
        out.push_str(newline);
    }
    out
}
pub fn sanitize_label(text: &Value) -> String {
    if text.is_null() {
        String::new()
    } else {
        no_controls(&string(text)).take(MAX_LABEL_LEN).collect()
    }
}
pub fn sanitize_metadata_string(value: &Value) -> String {
    let mut escaped = String::new();
    for c in no_controls(&string(value)) {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(c),
        }
    }
    escaped.chars().take(METADATA_MAX_VALUE_LEN).collect()
}
pub fn sanitize_metadata_value(value: &Value) -> Value {
    match value {
        Value::String(_) => Value::String(sanitize_metadata_string(value)),
        Value::Object(_) => Value::Object(sanitize_metadata(Some(value))),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(METADATA_MAX_LIST_ITEMS)
                .map(sanitize_metadata_value)
                .collect(),
        ),
        _ => value.clone(),
    }
}
pub fn sanitize_metadata(value: Option<&Value>) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(Value::Object(metadata)) = value {
        for (key, value) in metadata {
            let key = sanitize_metadata_string(&Value::String(key.clone()));
            if !key.is_empty() {
                result.insert(key, sanitize_metadata_value(value));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    // Graphify 0.9.93 tests/test_security.py, `redact_secrets` cases.
    #[test]
    fn redact_secrets_replaces_credential_shapes_only() {
        for secret in [
            format!("ghp_{}", "a".repeat(36)),
            "AKIAIOSFODNN7EXAMPLE".to_owned(),
            format!("sk_live_{}", "x".repeat(24)),
            "xoxb-1234567890-abcdefghij".to_owned(),
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk".to_owned(),
        ] {
            let out = redact_secrets(&format!("see {secret} here"));
            assert!(
                !out.contains(&secret) && out.contains(REDACTED_MARKER),
                "{secret}"
            );
        }
        assert_eq!(
            redact_secrets("postgres://svc:hunter2@db.internal/app"),
            "postgres://[redacted]@db.internal/app"
        );
        assert_eq!(
            redact_secrets("Authorization: Bearer abcDEF123456ghiJKL789mnop"),
            "Authorization: Bearer [redacted]"
        );
        for benign in [
            "use chunked processing; see https://example.com/a/b",
            "see commit 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b for context",
            "digest sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "run id 550e8400-e29b-41d4-a716-446655440000 completed",
            "class AkiaConfigProvider implements Provider",
            "",
        ] {
            assert_eq!(redact_secrets(benign), benign);
        }
    }
    /// A long docstring of ordinary prose comes back whole: the backtracking
    /// limit applies per line, and a whole-text search of 200,000 characters
    /// exhausted it (the text came back as `[redacted]`). Secrets on any line
    /// are still found, and line breaks are kept.
    #[test]
    fn a_long_docstring_is_searched_line_by_line() {
        let prose = "This module explains how the parser handles nested blocks and why.\n";
        let long = prose.repeat(200_000 / prose.len() + 1);
        assert!(long.len() > 200_000);
        assert_eq!(redact_secrets(&long), long);
        let token = format!("ghp_{}", "a".repeat(36));
        let text = format!("{long}token {token}\r\nend\n\nBearer abcDEF123456ghiJKL789mnop");
        let out = redact_secrets(&text);
        assert_eq!(
            out,
            format!("{long}token [redacted]\r\nend\n\nBearer [redacted]")
        );
        // The same result as one whole-text search where that one finishes.
        let short = format!("{prose}see {token} and postgres://u:p@h/db\n{prose}");
        assert_eq!(
            redact_secrets(&short),
            SECRET_SHAPE
                .replace_all(&short, fancy_regex::NoExpand(REDACTED_MARKER))
                .into_owned()
        );
    }
    struct FixedDns(Vec<SocketAddr>);
    impl Resolver for FixedDns {
        fn resolve(&self, _: &str, _: u16) -> Result<Vec<SocketAddr>> {
            Ok(self.0.clone())
        }
    }
    struct Canned {
        status: u16,
        content: Vec<u8>,
        location: Option<String>,
        calls: usize,
    }
    impl Transport for Canned {
        fn get(&mut self, url: &Url, address: SocketAddr, _: Duration) -> Result<FetchResponse> {
            self.calls += 1;
            assert_eq!(url.host_str(), Some("example.com"));
            assert_eq!(address.ip(), "93.184.215.14".parse::<IpAddr>().unwrap());
            Ok(FetchResponse {
                status: self.status,
                location: self.location.clone(),
                body: Box::new(std::io::Cursor::new(self.content.clone())),
            })
        }
    }
    #[test]
    fn every_resolved_address_and_redirect_is_checked() {
        let dns = FixedDns(vec![
            "93.184.215.14:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ]);
        assert!(validate_url_with("https://example.com/", &dns).is_err());
        let public = FixedDns(vec!["93.184.215.14:443".parse().unwrap()]);
        let mut redirect = Canned {
            status: 302,
            location: Some("file:///etc/passwd".into()),
            content: vec![],
            calls: 0,
        };
        assert!(
            safe_fetch_with(
                "https://example.com/",
                100,
                Duration::from_secs(1),
                &public,
                &mut redirect
            )
            .unwrap_err()
            .to_string()
            .contains("Blocked URL scheme")
        );
        assert_eq!(redirect.calls, 1);
    }
    #[test]
    fn bounded_reads_reject_overflow_and_non_success_status() {
        let dns = FixedDns(vec!["93.184.215.14:443".parse().unwrap()]);
        let mut source = Canned {
            status: 200,
            location: None,
            content: b"hello".to_vec(),
            calls: 0,
        };
        assert_eq!(
            safe_fetch_with(
                "https://example.com/",
                5,
                Duration::from_secs(1),
                &dns,
                &mut source
            )
            .unwrap(),
            b"hello"
        );
        assert!(
            safe_fetch_with(
                "https://example.com/",
                4,
                Duration::from_secs(1),
                &dns,
                &mut source
            )
            .unwrap_err()
            .to_string()
            .contains("size limit")
        );
        source.status = 404;
        assert!(
            safe_fetch_with(
                "https://example.com/",
                100,
                Duration::from_secs(1),
                &dns,
                &mut source
            )
            .unwrap_err()
            .to_string()
            .starts_with("HTTPError:")
        );
    }
    /// Graphify 0.9.94-0.9.95 (#4643-#4645, #4708): credentials, bad ports
    /// and hostless URLs are refused before resolving, on the first fetch and
    /// on a redirect; multicast and unspecified addresses are blocked.
    #[test]
    fn malformed_and_credential_urls_and_multicast_hosts_are_refused() {
        let public = FixedDns(vec!["93.184.215.14:443".parse().unwrap()]);
        for (url, reason) in [
            ("http://", "without a hostname"),
            ("http:///etc/passwd", "without a hostname"),
            ("https:///example.com/path", "without a hostname"),
            ("http:example.com", "without a hostname"),
            ("http://example.com:abc/path", "with an invalid port"),
            ("https://example.com:65536/path", "with an invalid port"),
            ("http://example.com:0/path", "with port 0"),
            ("http://[::1]:0/", "with port 0"),
            (
                "https://user:secret@example.com/path",
                "containing embedded credentials",
            ),
            (
                "https://user@example.com/path",
                "containing embedded credentials",
            ),
        ] {
            let error = validate_url_with(url, &public).unwrap_err().to_string();
            assert!(
                error.contains(&format!("Blocked URL {reason}")),
                "{url}: {error}"
            );
        }
        for url in [
            "https://example.com:65535/a@b?c=@",
            "https://example.com/path@x",
            "http://example.com:0080/",
        ] {
            assert_eq!(validate_url_with(url, &public).unwrap(), url);
        }
        let mut redirect = Canned {
            status: 302,
            location: Some("https://user:pw@example.com/".into()),
            content: vec![],
            calls: 0,
        };
        let error = safe_fetch_with(
            "https://example.com/",
            100,
            Duration::from_secs(1),
            &public,
            &mut redirect,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("embedded credentials"), "{error}");
        // The Location is checked as written, as upstream's redirect handler
        // checks urllib's joined URL: an empty username is refused, not
        // normalised away, also scheme-relative; a relative path is followed.
        for location in [
            "https://@example.com/",
            "HTTPS://:@example.com/next",
            "//@example.com/next",
            " \thttps://@example.com/",
        ] {
            let mut redirect = Canned {
                status: 302,
                location: Some(location.into()),
                content: vec![],
                calls: 0,
            };
            let error = safe_fetch_with(
                "https://example.com/",
                100,
                Duration::from_secs(1),
                &public,
                &mut redirect,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("embedded credentials"),
                "{location}: {error}"
            );
            assert_eq!(redirect.calls, 1, "{location}");
        }
        let mut relative = Canned {
            status: 302,
            location: Some("/next".into()),
            content: vec![],
            calls: 0,
        };
        let error = safe_fetch_with(
            "https://example.com/",
            100,
            Duration::from_secs(1),
            &public,
            &mut relative,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("redirect loop"), "{error}");
        assert!(relative.calls > 1);
        for ip in ["224.0.0.1", "239.1.1.1", "ff02::1", "0.0.0.0", "::"] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!ip_is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }
}
