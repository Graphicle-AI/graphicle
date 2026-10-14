// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "ValueError: Blocked URL scheme '{scheme}' - only http and https are allowed. Got: {}",
            repr_string(url)
        )
        .into());
    }
    Ok(Url::parse(url)
        .map_err(|error| format!("ValueError: Invalid URL {}: {error}", repr_string(url)))?)
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
    format!("URLError: {}", details.join(": "))
}
impl Transport for PinnedHttpTransport {
    fn get(&mut self, url: &Url, address: SocketAddr, timeout: Duration) -> Result<FetchResponse> {
        let host = url.host_str().ok_or("ValueError: URL has no hostname")?;
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .resolve_to_addrs(host, &[address])
            .user_agent(concat!("Mozilla/5.0 graphicle/", env!("CARGO_PKG_VERSION")))
            .build()
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

fn no_controls(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().filter(|c| *c > '\x1f' && *c != '\x7f')
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
}
