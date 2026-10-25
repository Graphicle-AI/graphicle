// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/ingest.py
//! Port of graphify/ingest.py: attributed URL capture and query-result persistence.
use crate::{
    Result,
    build::Diagnostics,
    python_compat, security,
    transcribe::{Downloader, NativeDownloader},
};
use chrono::{DateTime, SecondsFormat, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const OUTCOMES: &[&str] = &["useful", "dead_end", "corrected"];
pub fn yaml_str(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut result = String::new();
    for c in python_compat::string(value).chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            '\u{2028}' => result.push_str("\\L"),
            '\u{2029}' => result.push_str("\\P"),
            c if c < ' ' || c == '\x7f' => result.push_str(&format!("\\x{:02x}", c as u32)),
            _ => result.push(c),
        }
    }
    result
}
fn yaml(s: &str) -> String {
    yaml_str(&Value::String(s.into()))
}
pub fn iso_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(
        if time.timestamp_subsec_micros() == 0 {
            SecondsFormat::Secs
        } else {
            SecondsFormat::Micros
        },
        false,
    )
}
pub fn url_parts(input: &str) -> (String, String) {
    let url = input
        .trim_start_matches(|c: char| c <= ' ')
        .replace(['\r', '\n', '\t'], "");
    let url = url.split(['?', '#']).next().unwrap_or("");
    let (scheme, rest) = url
        .split_once(':')
        .filter(|(s, _)| {
            s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
        })
        .unwrap_or(("", url));
    let (netloc, path) = if let Some(rest) = rest.strip_prefix("//") {
        rest.find('/')
            .map_or((rest, ""), |at| (&rest[..at], &rest[at..]))
    } else {
        ("", rest)
    };
    let last = path.rfind('/').map_or(0, |at| at + 1);
    let path = if [
        "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
        "sip", "sips", "mms", "sftp", "tel",
    ]
    .contains(&scheme.to_ascii_lowercase().as_str())
    {
        path[last..].find(';').map_or(path, |at| &path[..last + at])
    } else {
        path
    };
    (netloc.into(), path.into())
}
pub fn safe_filename(url: &str, suffix: &str) -> String {
    let (host, path) = url_parts(url);
    let pattern = crate::regex_compat::compile(r"[^\w\-]");
    let name = pattern
        .replace_all(&(host + &path), "_")
        .trim_matches('_')
        .to_owned();
    let name = regex::Regex::new("_+").unwrap().replace_all(&name, "_");
    name.chars().take(80).collect::<String>() + suffix
}
/// `urllib.parse.urlsplit(url).hostname`, raising where `urlsplit` raises
/// (`ValueError` for malformed bracketed hosts and NFKC-unsafe netlocs).
pub fn url_hostname(url: &str) -> Result<Option<String>> {
    let (netloc, _) = url_parts(url);
    let (open, close) = (netloc.contains('['), netloc.contains(']'));
    if open != close {
        return Err("ValueError: Invalid IPv6 URL".into());
    }
    let hostinfo = netloc.rsplit_once('@').map_or(netloc.as_str(), |(_, h)| h);
    let hostname = match hostinfo.split_once('[') {
        Some((before, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !before.is_empty() || (!after.is_empty() && !after.starts_with(':')) {
                return Err("ValueError: Invalid IPv6 URL".into());
            }
            host
        }
        None => hostinfo.split_once(':').map_or(hostinfo, |(h, _)| h),
    };
    if open {
        check_bracketed_host(hostname)?;
    }
    check_netloc_normalization(&netloc)?;
    if hostname.is_empty() {
        return Ok(None);
    }
    // Scoped IPv6 zone ids keep their case.
    let (host, zone) = hostname
        .split_once('%')
        .map_or((hostname, None), |(h, z)| (h, Some(z)));
    Ok(Some(match zone {
        Some(zone) => format!("{}%{zone}", host.to_lowercase()),
        None => host.to_lowercase(),
    }))
}
/// `urllib.parse._check_bracketed_host`.
fn check_bracketed_host(hostname: &str) -> Result<()> {
    if let Some(future) = hostname.strip_prefix('v') {
        let valid = future.split_once('.').is_some_and(|(hex, rest)| {
            !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !rest.is_empty()
        }) && !hostname.contains('\n');
        return if valid {
            Ok(())
        } else {
            Err("ValueError: IPvFuture address is invalid".into())
        };
    }
    let (address, scope) = hostname
        .split_once('%')
        .map_or((hostname, None), |(a, s)| (a, Some(s)));
    let ipv6 = address.parse::<std::net::Ipv6Addr>().is_ok()
        && scope.is_none_or(|s| !s.is_empty() && !s.contains('%'));
    if ipv6 {
        return Ok(());
    }
    if scope.is_none() && address.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err("ValueError: An IPv4 address cannot be in brackets".into());
    }
    Err(format!(
        "ValueError: {} does not appear to be an IPv4 or IPv6 address",
        python_compat::repr_string(hostname)
    )
    .into())
}
/// `urllib.parse._checknetloc`: a netloc whose NFKC form introduces URL
/// delimiters is rejected.
fn check_netloc_normalization(netloc: &str) -> Result<()> {
    use unicode_normalization::UnicodeNormalization;
    if netloc.is_ascii() {
        return Ok(());
    }
    let stripped: String = netloc.chars().filter(|c| !"@:#?".contains(*c)).collect();
    let normalized: String = stripped.nfkc().collect();
    if stripped != normalized && normalized.chars().any(|c| "/?#@:".contains(c)) {
        return Err(format!(
            "ValueError: netloc {} contains invalid characters under NFKC normalization",
            python_compat::repr_string(netloc)
        )
        .into());
    }
    Ok(())
}
fn host_is(host: &str, domains: &[&str]) -> bool {
    domains.iter().any(|domain| {
        host == *domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
    })
}
/// Python 3.12 `urllib.parse.urlsplit`: (scheme, netloc, path, query,
/// fragment), raising where it raises.
fn urlsplit(input: &str) -> Result<[String; 5]> {
    let mut url: String = input.trim_start_matches(|c: char| c <= ' ').to_owned();
    url.retain(|c| !matches!(c, '\t' | '\r' | '\n'));
    let mut scheme = String::new();
    if let Some(i) = url.find(':')
        && i > 0
        && url.as_bytes()[0].is_ascii_alphabetic()
        && url[..i]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    {
        scheme = url[..i].to_ascii_lowercase();
        url = url[i + 1..].to_owned();
    }
    let mut netloc = String::new();
    if let Some(rest) = url.strip_prefix("//") {
        let delim = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        netloc = rest[..delim].to_owned();
        url = rest[delim..].to_owned();
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err("ValueError: Invalid IPv6 URL".into());
        }
        if open {
            let bracketed = netloc.split_once('[').map_or("", |(_, b)| b);
            check_bracketed_host(bracketed.split_once(']').map_or(bracketed, |(h, _)| h))?;
        }
    }
    let mut fragment = String::new();
    if let Some((before, after)) = url.split_once('#') {
        fragment = after.to_owned();
        url = before.to_owned();
    }
    let mut query = String::new();
    if let Some((before, after)) = url.split_once('?') {
        query = after.to_owned();
        url = before.to_owned();
    }
    check_netloc_normalization(&netloc)?;
    Ok([scheme, netloc, url, query, fragment])
}
/// Python 3.12 `urllib.parse.urlunsplit`.
fn urlunsplit([scheme, netloc, path, query, fragment]: &[String; 5]) -> String {
    const USES_NETLOC: &[&str] = &[
        "",
        "ftp",
        "http",
        "gopher",
        "nntp",
        "telnet",
        "imap",
        "wais",
        "file",
        "mms",
        "https",
        "shttp",
        "snews",
        "prospero",
        "rtsp",
        "rtsps",
        "rtspu",
        "rsync",
        "svn",
        "svn+ssh",
        "sftp",
        "nfs",
        "git",
        "git+ssh",
        "ws",
        "wss",
        "itms-services",
    ];
    let mut url = path.clone();
    if !netloc.is_empty() {
        if !url.is_empty() && !url.starts_with('/') {
            url.insert(0, '/');
        }
        url = format!("//{netloc}{url}");
    } else if url.starts_with("//")
        || (!scheme.is_empty()
            && USES_NETLOC.contains(&scheme.as_str())
            && (url.is_empty() || url.starts_with('/')))
    {
        url = format!("//{url}");
    }
    if !scheme.is_empty() {
        url = format!("{scheme}:{url}");
    }
    if !query.is_empty() {
        url = format!("{url}?{query}");
    }
    if !fragment.is_empty() {
        url = format!("{url}#{fragment}");
    }
    url
}
/// `SplitResult._hostinfo` as (hostname, port) of a netloc.
fn hostinfo(netloc: &str) -> (&str, Option<&str>) {
    let info = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (host, port) = match info.split_once('[') {
        Some((_, bracketed)) => {
            let (host, rest) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, rest.split_once(':').map_or("", |(_, p)| p))
        }
        None => info.split_once(':').unwrap_or((info, "")),
    };
    (host, (!port.is_empty()).then_some(port))
}
/// The oEmbed URL for a tweet (0.9.71, #3880): rewrite only an `x.com` host
/// to `twitter.com`, never `x.com` text elsewhere in the URL.
fn tweet_oembed_url(url: &str) -> Result<String> {
    let mut parts = urlsplit(url)?;
    let (hostname, port) = hostinfo(&parts[1]);
    // `SplitResult.hostname`: lowercased, a scoped IPv6 zone keeps its case.
    let hostname = match hostname.split_once('%') {
        Some((h, zone)) => format!("{}%{zone}", h.to_lowercase()),
        None => hostname.to_lowercase(),
    };
    let host = hostname.trim_end_matches('.');
    if host_is(host, &["x.com"]) {
        let port = match port {
            Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => {
                // Python's int() of an ASCII digit run; the range check follows.
                let value = p.trim_start_matches('0');
                if value.len() > 5 || value.parse::<u32>().unwrap_or(0) > 65535 {
                    return Err("ValueError: Port out of range 0-65535".into());
                }
                value.parse::<u32>().unwrap_or(0)
            }
            Some(p) => {
                return Err(format!(
                    "ValueError: Port could not be cast to integer value as {}",
                    python_compat::repr_string(p)
                )
                .into());
            }
            None => 0,
        };
        let host = format!("{}twitter.com", &host[..host.len() - "x.com".len()]);
        parts[1] = if port != 0 {
            format!("{host}:{port}")
        } else {
            host
        };
    }
    Ok(urlunsplit(&parts))
}
/// Classify a URL by its parsed host, not by text anywhere in it (#3831).
pub fn detect_url_type(url: &str) -> Result<&'static str> {
    let hostname = url_hostname(url)?.unwrap_or_default();
    let host = hostname.trim_end_matches('.');
    if host_is(host, &["twitter.com", "x.com"]) {
        return Ok("tweet");
    }
    if host_is(host, &["arxiv.org"]) {
        return Ok("arxiv");
    }
    if host_is(host, &["github.com"]) {
        return Ok("github");
    }
    if host_is(host, &["youtube.com", "youtu.be"]) {
        return Ok("youtube");
    }
    let path = url_parts(url).1.to_lowercase();
    if path.ends_with(".pdf") {
        return Ok("pdf");
    }
    if [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|s| path.ends_with(s))
    {
        return Ok("image");
    }
    Ok("webpage")
}
#[derive(Clone, Copy, Debug, Default)]
pub enum HtmlMode {
    #[default]
    Markdown,
    Basic,
}
pub fn html_to_markdown(html: &str, mode: HtmlMode) -> Result<String> {
    let html = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .unwrap()
        .replace_all(html, "");
    let html = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .unwrap()
        .replace_all(&html, "");
    if matches!(mode, HtmlMode::Basic) {
        let text = regex::Regex::new(r"<[^>]+>")
            .unwrap()
            .replace_all(&html, " ");
        let text = crate::regex_compat::compile(r"\s+").replace_all(&text, " ");
        return Ok(text
            .trim_matches(crate::dedup_strings::whitespace)
            .chars()
            .take(8000)
            .collect());
    }
    let html = regex::Regex::new(r"(?is)<img\b[^>]*>")
        .unwrap()
        .replace_all(&html, "");
    Ok(html2text::config::plain()
        .no_link_wrapping()
        .link_footnotes(true)
        .allow_width_overflow()
        .string_from_read(html.as_bytes(), 12000)
        .map_err(|e| format!("RuntimeError: HTML conversion failed: {e}"))?)
}
pub trait Fetcher: Send + Sync {
    fn validate(&self, url: &str) -> Result<()>;
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>>;
    fn text(&self, url: &str) -> Result<String> {
        self.fetch(url, security::MAX_TEXT_BYTES)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}
pub struct NativeFetcher;
impl Fetcher for NativeFetcher {
    fn validate(&self, url: &str) -> Result<()> {
        security::validate_url(url).map(|_| ())
    }
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
        security::safe_fetch(url, max_bytes, Duration::from_secs(30))
    }
}
pub struct Runtime {
    pub fetcher: Arc<dyn Fetcher>,
    pub downloader: Arc<dyn Downloader>,
    pub html_mode: HtmlMode,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            fetcher: Arc::new(NativeFetcher),
            downloader: Arc::new(NativeDownloader {
                env: std::env::vars().collect(),
            }),
            html_mode: HtmlMode::Markdown,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureOptions {
    pub author: Option<String>,
    pub contributor: Option<String>,
}
impl CaptureOptions {
    fn contributor(&self) -> &str {
        self.contributor
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.author.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or("unknown")
    }
}
fn quote_url(url: &str) -> String {
    let mut s = String::new();
    for b in url.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}
fn strip_tags(s: &str, replacement: &str) -> String {
    regex::Regex::new(r"<[^>]+>")
        .unwrap()
        .replace_all(s, replacement)
        .trim_matches(crate::dedup_strings::whitespace)
        .to_owned()
}
impl Runtime {
    pub fn fetch_tweet(
        &self,
        url: &str,
        options: &CaptureOptions,
        now: DateTime<Utc>,
    ) -> Result<(String, String)> {
        let api = format!(
            "https://publish.twitter.com/oembed?url={}&omit_script=true",
            quote_url(&tweet_oembed_url(url)?)
        );
        let result = self
            .fetcher
            .text(&api)
            .and_then(|s| serde_json::from_str::<Value>(&s).map_err(crate::Error::from))
            .and_then(|v| {
                let html = v
                    .get("html")
                    .unwrap_or(&Value::String(String::new()))
                    .as_str()
                    .ok_or("Invalid tweet HTML")?
                    .to_owned();
                Ok((
                    strip_tags(&html, ""),
                    v.get("author_name")
                        .cloned()
                        .unwrap_or(Value::String("unknown".into())),
                ))
            });
        let (body, author) = result.unwrap_or_else(|_| {
            (
                format!("Tweet at {url} (could not fetch content)"),
                Value::String("unknown".into()),
            )
        });
        let content = format!(
            "---\nsource_url: \"{}\"\ntype: tweet\nauthor: \"{}\"\ncaptured_at: {}\ncontributor: \"{}\"\n---\n\n# Tweet by @{}\n\n{body}\n\nSource: {url}\n",
            yaml(url),
            yaml_str(&author),
            iso_time(now),
            yaml(options.contributor()),
            python_compat::string(&author)
        );
        Ok((content, safe_filename(url, ".md")))
    }
    pub fn fetch_webpage(
        &self,
        url: &str,
        options: &CaptureOptions,
        now: DateTime<Utc>,
    ) -> Result<(String, String)> {
        let html = self.fetcher.text(url)?;
        let re = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap();
        let title = re
            .captures(&html)
            .map(|c| {
                crate::regex_compat::compile(r"\s+")
                    .replace_all(&c[1], " ")
                    .trim_matches(crate::dedup_strings::whitespace)
                    .to_owned()
            })
            .unwrap_or_else(|| url.into());
        let markdown = html_to_markdown(&html, self.html_mode)?
            .chars()
            .take(12000)
            .collect::<String>();
        let content = format!(
            "---\nsource_url: \"{}\"\ntype: webpage\ntitle: \"{}\"\ncaptured_at: {}\ncontributor: \"{}\"\n---\n\n# {title}\n\nSource: {url}\n\n---\n\n{markdown}\n",
            yaml(url),
            yaml(&title),
            iso_time(now),
            yaml(options.contributor())
        );
        Ok((content, safe_filename(url, ".md")))
    }
    pub fn fetch_arxiv(
        &self,
        url: &str,
        options: &CaptureOptions,
        now: DateTime<Utc>,
    ) -> Result<(String, String)> {
        let re = crate::regex_compat::compile(r"(\d{4}\.\d{4,5})");
        let Some(id) = re.captures(url).map(|c| c[1].to_owned()) else {
            return self.fetch_webpage(url, options, now);
        };
        let html = self
            .fetcher
            .text(&format!("https://export.arxiv.org/abs/{id}"))
            .unwrap_or_default();
        let field = |pattern: &str, replacement: &str| {
            regex::Regex::new(pattern)
                .unwrap()
                .captures(&html)
                .map(|c| strip_tags(&c[1], replacement))
        };
        let title =
            field(r#"(?is)class="title[^"]*"[^>]*>(.*?)</h1>"#, " ").unwrap_or_else(|| id.clone());
        let body =
            field(r#"(?is)class="abstract[^"]*"[^>]*>(.*?)</blockquote>"#, "").unwrap_or_default();
        let authors = field(r#"(?is)class="authors"[^>]*>(.*?)</div>"#, "").unwrap_or_default();
        let content = format!(
            "---\nsource_url: \"{}\"\narxiv_id: \"{}\"\ntype: paper\ntitle: \"{}\"\npaper_authors: \"{}\"\ncaptured_at: {}\ncontributor: \"{}\"\n---\n\n# {title}\n\n**Authors:** {authors}\n**arXiv:** {id}\n\n## Abstract\n\n{body}\n\nSource: {url}\n",
            yaml(url),
            yaml(&id),
            yaml(&title),
            yaml(&authors),
            iso_time(now),
            yaml(options.contributor())
        );
        Ok((content, format!("arxiv_{}.md", id.replace('.', "_"))))
    }
    /// Save a sequential capture without replacing an existing directory entry
    /// (#4393): a taken name, even a dangling symlink, moves the capture to
    /// `<stem>_<n><suffix>`. Selection and writing are separate; concurrent
    /// captures are not atomic.
    pub fn download_binary(&self, url: &str, suffix: &str, target: &Path) -> Result<PathBuf> {
        let filename = safe_filename(url, suffix);
        let stem = Path::new(&filename)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut path = target.join(&filename);
        let mut counter = 1usize;
        loop {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    path = target.join(format!("{stem}_{counter}{suffix}"));
                    counter += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(format!("OSError: {e}").into()),
            }
        }
        let bytes = self.fetcher.fetch(url, security::MAX_FETCH_BYTES)?;
        std::fs::write(&path, bytes).map_err(|e| format!("OSError: {e}"))?;
        Ok(path)
    }
    pub fn ingest(
        &self,
        url: &str,
        target: &Path,
        options: &CaptureOptions,
        d: &mut Diagnostics,
    ) -> Result<PathBuf> {
        self.ingest_at(url, target, options, Utc::now(), d)
    }
    pub fn ingest_at(
        &self,
        url: &str,
        target: &Path,
        options: &CaptureOptions,
        now: DateTime<Utc>,
        d: &mut Diagnostics,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(target).map_err(|e| format!("OSError: {e}"))?;
        self.fetcher.validate(url).map_err(|e| {
            let e = e.to_string();
            format!(
                "ValueError: ingest: {}",
                e.strip_prefix("ValueError: ").unwrap_or(&e)
            )
        })?;
        let kind = detect_url_type(url)?;
        let result: Result<PathBuf> = (|| {
            let downloaded = match kind {
                "pdf" => Some(("PDF", self.download_binary(url, ".pdf", target)?)),
                "image" => {
                    let url_path = url_parts(url).1;
                    let ext = Path::new(&url_path)
                        .extension()
                        .map(|s| format!(".{}", s.to_string_lossy()))
                        .unwrap_or_else(|| ".jpg".into());
                    Some(("image", self.download_binary(url, &ext, target)?))
                }
                "youtube" => Some(("audio", self.downloader.download(url, target, d)?)),
                _ => None,
            };
            if let Some((label, path)) = downloaded {
                d.stdout.push(format!(
                    "Downloaded {label}: {}",
                    path.file_name().unwrap().to_string_lossy()
                ));
                return Ok(path);
            }
            let (content, filename) = match kind {
                "tweet" => self.fetch_tweet(url, options, now)?,
                "arxiv" => self.fetch_arxiv(url, options, now)?,
                _ => self.fetch_webpage(url, options, now)?,
            };
            // `_free_capture_path` (Graphify 0.9.94, #4657): the name, else the
            // first free `<stem>_<n><suffix>`. Creating with O_EXCL never writes
            // through an existing entry, a dangling symlink included, and takes
            // the free name atomically.
            let stem = Path::new(&filename)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let suffix = Path::new(&filename)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            for counter in 0usize.. {
                let path = target.join(if counter == 0 {
                    filename.clone()
                } else {
                    format!("{stem}_{counter}{suffix}")
                });
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        file.write_all(content.as_bytes())
                            .map_err(|e| format!("OSError: {e}"))?;
                        d.stdout.push(format!(
                            "Saved {kind}: {}",
                            path.file_name().unwrap().to_string_lossy()
                        ));
                        return Ok(path);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(format!("OSError: {e}").into()),
                }
            }
            unreachable!("the capture name counter is unbounded")
        })();
        Ok(result.map_err(|e| {
            let e = e.to_string();
            if ["HTTPError: ", "URLError: ", "OSError: "]
                .iter()
                .any(|p| e.starts_with(p))
            {
                let message = e
                    .split_once(": ")
                    .map_or(e.as_str(), |(_, message)| message);
                format!(
                    "RuntimeError: ingest: failed to fetch {}: {message}",
                    python_compat::repr_string(url)
                )
            } else {
                e
            }
        })?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveOptions {
    pub query_type: String,
    pub source_nodes: Vec<String>,
    pub outcome: Option<String>,
    pub correction: Option<String>,
}
impl Default for SaveOptions {
    fn default() -> Self {
        Self {
            query_type: "query".into(),
            source_nodes: vec![],
            outcome: None,
            correction: None,
        }
    }
}
pub fn query_content(
    question: &str,
    answer: &str,
    options: &SaveOptions,
    now: DateTime<Utc>,
) -> Result<String> {
    if let Some(outcome) = options.outcome.as_deref()
        && !OUTCOMES.contains(&outcome)
    {
        return Err(format!(
            "ValueError: outcome must be one of ('useful', 'dead_end', 'corrected'), got {}",
            python_compat::repr_string(outcome)
        )
        .into());
    }
    let mut lines = vec![
        "---".into(),
        format!("type: \"{}\"", yaml(&options.query_type)),
        format!("date: \"{}\"", iso_time(now)),
        format!("question: \"{}\"", yaml(question)),
        "contributor: \"graphicle\"".into(),
    ];
    if let Some(outcome) = options.outcome.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("outcome: \"{}\"", yaml(outcome)));
    }
    if let Some(correction) = options.correction.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("correction: \"{}\"", yaml(correction)));
    }
    if !options.source_nodes.is_empty() {
        lines.push(format!(
            "source_nodes: [{}]",
            options
                .source_nodes
                .iter()
                .take(10)
                .map(|s| format!("\"{}\"", yaml(s)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.extend([
        "---".into(),
        "".into(),
        format!("# Q: {question}"),
        "".into(),
        "## Answer".into(),
        "".into(),
        answer.into(),
    ]);
    if options.outcome.as_deref().is_some_and(|s| !s.is_empty())
        || options.correction.as_deref().is_some_and(|s| !s.is_empty())
    {
        lines.extend(["".into(), "## Outcome".into(), "".into()]);
        if let Some(outcome) = options.outcome.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("- Signal: {outcome}"));
        }
        if let Some(correction) = options.correction.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("- Correction: {correction}"));
        }
    }
    if !options.source_nodes.is_empty() {
        lines.extend(["".into(), "## Source Nodes".into(), "".into()]);
        lines.extend(options.source_nodes.iter().map(|s| format!("- {s}")));
    }
    Ok(lines.join("\n"))
}
pub fn query_filename(question: &str, now: DateTime<Utc>, uuid_prefix: &str) -> Result<String> {
    if uuid_prefix.len() != 8 || !uuid_prefix.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("ValueError: expected eight hexadecimal UUID characters".into());
    }
    let pattern = crate::regex_compat::compile(r"[^\w]");
    let slug = pattern
        .replace_all(&question.to_lowercase(), "_")
        .chars()
        .take(50)
        .collect::<String>()
        .trim_matches('_')
        .to_owned();
    Ok(format!(
        "query_{}_{}_{slug}.md",
        now.format("%Y%m%d_%H%M%S"),
        uuid_prefix
    ))
}
pub fn save_query_result(
    question: &str,
    answer: &str,
    memory_dir: &Path,
    options: &SaveOptions,
) -> Result<PathBuf> {
    let now = Utc::now();
    let content = query_content(question, answer, options, now)?;
    std::fs::create_dir_all(memory_dir).map_err(|e| format!("OSError: {e}"))?;
    for _ in 0..100 {
        let mut random = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut random);
        let prefix = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = memory_dir.join(query_filename(question, now, &prefix)?);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(content.as_bytes())
                    .map_err(|e| format!("OSError: {e}"))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("OSError: {e}").into()),
        }
    }
    Err("OSError: could not reserve a unique query filename".into())
}

#[cfg(test)]
mod tweet_url_tests {
    use super::tweet_oembed_url;

    #[test]
    fn oembed_rewrites_only_the_host_like_python() {
        // Expected values printed by CPython 3.12.13 running 0.9.71's
        // `_fetch_tweet` rewrite (urlsplit -> host swap -> urlunsplit).
        for (url, expected) in [
            (
                "https://x.com/user/status/1",
                "https://twitter.com/user/status/1",
            ),
            (
                "https://mobile.x.com/user/status/1",
                "https://mobile.twitter.com/user/status/1",
            ),
            (
                "https://X.COM/user/status/1",
                "https://twitter.com/user/status/1",
            ),
            (
                "https://x.com/user/status/1?next=https://x.com/home",
                "https://twitter.com/user/status/1?next=https://x.com/home",
            ),
            (
                "https://twitter.com/dropbox.com_fan/status/1",
                "https://twitter.com/dropbox.com_fan/status/1",
            ),
            (
                "https://user:pw@x.com:8443/a?",
                "https://twitter.com:8443/a",
            ),
            ("https://x.com.:0/a#frag", "https://twitter.com/a#frag"),
            ("https://x.com:00443/s", "https://twitter.com:443/s"),
            ("HTTPS://Twitter.com/a?", "https://Twitter.com/a"),
            ("  https://x.com/a\tb", "https://twitter.com/ab"),
            ("x.com/user/status/1", "x.com/user/status/1"),
            ("//x.com/a", "//twitter.com/a"),
            ("https://notx.com/a", "https://notx.com/a"),
            ("https://x.com", "https://twitter.com"),
            ("https://x.com?q=1", "https://twitter.com?q=1"),
        ] {
            assert_eq!(tweet_oembed_url(url).unwrap(), expected, "{url}");
        }
        for (url, message) in [
            (
                "https://x.com:abc/a",
                "Port could not be cast to integer value as 'abc'",
            ),
            ("https://x.com:70000/a", "Port out of range 0-65535"),
            ("https://[::1/a", "Invalid IPv6 URL"),
        ] {
            let error = tweet_oembed_url(url).unwrap_err().to_string();
            assert!(error.contains(message), "{url}: {error}");
        }
    }
}
