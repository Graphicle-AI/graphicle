// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Streamable HTTP MCP transport with API keys and expiring sessions.
use crate::{Result, serve::Server};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpOptions {
    pub host: String,
    pub port: u16,
    pub api_key: Option<String>,
    pub path: String,
    pub json_response: bool,
    pub stateless: bool,
    pub session_timeout: Option<f64>,
}
impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            api_key: None,
            path: "/mcp".into(),
            json_response: false,
            stateless: false,
            session_timeout: Some(3600.0),
        }
    }
}
pub fn authorized(headers: &BTreeMap<String, String>, key: &str) -> bool {
    let supplied = if let Some(k) = headers.get("x-api-key") {
        Some(k.as_str())
    } else {
        headers
            .get("authorization")
            .and_then(|s| s.split_once(' '))
            .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.is_empty())
            .map(|(_, token)| token.trim())
    };
    supplied.is_some_and(|s| {
        let mut diff = s.len() ^ key.len();
        for (a, b) in s.as_bytes().iter().zip(key.as_bytes()) {
            diff |= usize::from(a ^ b);
        }
        diff == 0
    })
}
/// The DNS-rebinding guard: a bind address other than a wildcard accepts only
/// its own Host header, `localhost` or `127.0.0.1`, each bare or with the
/// port. An IPv6 literal appears bracketed in the Host header (`[::1]:8080`),
/// so an IPv6 bind accepts that form (Graphify 0.9.94, #4656).
pub fn host_allowed(header: &str, options: &HttpOptions) -> bool {
    if ["0.0.0.0", "::", ""].contains(&options.host.as_str()) {
        return true;
    }
    let bind = if options.host.contains(':') {
        format!("[{}]", options.host.trim_matches(['[', ']']))
    } else {
        options.host.clone()
    };
    [bind.as_str(), "localhost", "127.0.0.1"]
        .iter()
        .any(|host| header == *host || header == format!("{host}:{}", options.port))
}
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn request(stream: &mut TcpStream) -> Result<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(crate::Error::from)?;
    let mut data = vec![];
    let mut buf = [0u8; 8192];
    let end = loop {
        let n = stream.read(&mut buf).map_err(crate::Error::from)?;
        if n == 0 {
            return Err("incomplete HTTP request".into());
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|s| s == b"\r\n\r\n") {
            if i > 65536 {
                return Err("HTTP headers too large".into());
            }
            break i + 4;
        }
        if data.len() > 65536 {
            return Err("HTTP headers too large".into());
        }
    };
    let head = std::str::from_utf8(&data[..end]).map_err(|_| "HTTP headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or("").split_whitespace();
    let method = start.next().unwrap_or("").to_string();
    let path = start.next().unwrap_or("").to_string();
    if start.next() != Some("HTTP/1.1") && !head.starts_with(&format!("{method} {path} HTTP/1.0")) {
        return Err("Unsupported HTTP version".into());
    }
    let mut headers = BTreeMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (k, v) = line.split_once(':').ok_or("Invalid HTTP header")?;
        let k = k.to_ascii_lowercase();
        if k == "content-length" && headers.contains_key(&k) {
            return Err("Duplicate Content-Length".into());
        }
        headers.insert(k, v.trim().to_string());
    }
    if let Some(encoding) = headers.get("transfer-encoding") {
        if headers.contains_key("content-length") || !encoding.eq_ignore_ascii_case("chunked") {
            return Err("Invalid Transfer-Encoding or conflicting Content-Length".into());
        }
        let body = read_chunked(stream, data[end..].to_vec())?;
        return Ok(Request {
            method,
            path,
            headers,
            body,
        });
    }
    let len = headers
        .get("content-length")
        .map(|v| v.parse::<usize>().map_err(|_| "Invalid Content-Length"))
        .transpose()?
        .unwrap_or(0);
    if len > 16 * 1024 * 1024 {
        return Err(crate::Error::LimitExceeded {
            resource: "HTTP body",
            limit: 16 * 1024 * 1024,
        });
    }
    let mut body = data[end..].to_vec();
    if body.len() > len {
        body.truncate(len);
    }
    while body.len() < len {
        let wanted = (len - body.len()).min(buf.len());
        let n = stream
            .read(&mut buf[..wanted])
            .map_err(crate::Error::from)?;
        if n == 0 {
            return Err("Incomplete HTTP body".into());
        }
        body.extend_from_slice(&buf[..n]);
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}
fn read_chunked(stream: &mut TcpStream, mut buffered: Vec<u8>) -> Result<Vec<u8>> {
    fn fill(stream: &mut TcpStream, buf: &mut Vec<u8>, needed: usize) -> Result<()> {
        while buf.len() < needed {
            let mut chunk = [0u8; 8192];
            let n = stream.read(&mut chunk).map_err(crate::Error::from)?;
            if n == 0 {
                return Err("Incomplete chunked body".into());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok(())
    }
    fn line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Vec<u8>> {
        loop {
            if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                if i > 8192 {
                    return Err("Chunk header too large".into());
                }
                let result = buf[..i].to_vec();
                buf.drain(..i + 2);
                return Ok(result);
            }
            if buf.len() > 8192 {
                return Err("Chunk header too large".into());
            }
            let want = buf.len() + 1;
            fill(stream, buf, want)?;
        }
    }
    let mut body = vec![];
    loop {
        let header = line(stream, &mut buffered)?;
        let header = std::str::from_utf8(&header).map_err(|_| "Invalid chunk size")?;
        let size = usize::from_str_radix(header.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "Invalid chunk size")?;
        if size == 0 {
            let mut trailer_size = 0;
            loop {
                let trailer = line(stream, &mut buffered)?;
                trailer_size += trailer.len();
                if trailer_size > 65536 {
                    return Err("HTTP trailers too large".into());
                }
                if trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        if size > 16 * 1024 * 1024 - body.len() {
            return Err(crate::Error::LimitExceeded {
                resource: "HTTP body",
                limit: 16 * 1024 * 1024,
            });
        }
        fill(stream, &mut buffered, size + 2)?;
        if &buffered[size..size + 2] != b"\r\n" {
            return Err("Invalid chunk terminator".into());
        }
        body.extend_from_slice(&buffered[..size]);
        buffered.drain(..size + 2);
    }
}
fn respond(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    body: &[u8],
    headers: &[(String, String)],
) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        let _ = write!(stream, "{k}: {v}\r\n");
    }
    let _ = write!(stream, "\r\n");
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
struct Session {
    last: Instant,
}
pub struct HttpServer {
    pub server: Arc<Server>,
    pub options: HttpOptions,
    sessions: Mutex<BTreeMap<String, Session>>,
}
impl HttpServer {
    pub fn new(server: Arc<Server>, mut options: HttpOptions) -> Result<Self> {
        if !options.path.starts_with('/') || options.path.contains(['\r', '\n', '?', '#']) {
            return Err(
                "HTTP mount path must start with '/' and contain no query or fragment".into(),
            );
        }
        options.api_key = options
            .api_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Self {
            server,
            options,
            sessions: Mutex::new(BTreeMap::new()),
        })
    }
    pub fn handle(&self, mut stream: TcpStream) {
        let r = match request(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                respond(
                    &mut stream,
                    if matches!(e, crate::Error::LimitExceeded { .. }) {
                        413
                    } else {
                        400
                    },
                    "application/json",
                    json!({"error":e}).to_string().as_bytes(),
                    &[],
                );
                return;
            }
        };
        let error = |stream: &mut TcpStream, status, msg: &str| {
            respond(
                stream,
                status,
                "application/json",
                json!({"error":msg}).to_string().as_bytes(),
                &[],
            )
        };
        if self
            .options
            .api_key
            .as_deref()
            .is_some_and(|key| !authorized(&r.headers, key))
        {
            respond(
                &mut stream,
                401,
                "application/json",
                b"{\"error\": \"unauthorized\"}",
                &[],
            );
            return;
        }
        if !host_allowed(
            r.headers.get("host").map(String::as_str).unwrap_or(""),
            &self.options,
        ) {
            error(&mut stream, 403, "Invalid Host header");
            return;
        }
        if let Some(origin) = r.headers.get("origin") {
            let host = origin
                .split_once("://")
                .map(|(_, s)| s.split('/').next().unwrap_or(""))
                .unwrap_or("");
            if !host_allowed(host, &self.options) {
                error(&mut stream, 403, "Invalid Origin header");
                return;
            }
        }
        if r.path.split('?').next() != Some(self.options.path.as_str()) {
            error(&mut stream, 404, "Not found");
            return;
        }
        if !["POST", "DELETE"].contains(&r.method.as_str()) {
            respond(
                &mut stream,
                405,
                "application/json",
                b"{\"error\":\"method not allowed\"}",
                &[("Allow".into(), "POST, DELETE".into())],
            );
            return;
        }
        let mut session_id = r.headers.get("mcp-session-id").cloned();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(timeout) = self
            .options
            .session_timeout
            .filter(|t| t.is_finite() && *t > 0.0)
        {
            sessions.retain(|_, s| s.last.elapsed().as_secs_f64() < timeout);
        }
        if r.method == "DELETE" {
            if self.options.stateless {
                error(&mut stream, 405, "Stateless server has no sessions");
            } else if session_id
                .as_ref()
                .is_some_and(|id| sessions.remove(id).is_some())
            {
                respond(&mut stream, 200, "application/json", b"{}", &[]);
            } else {
                error(&mut stream, 404, "Session not found");
            }
            return;
        }
        if !r.headers.get("content-type").is_some_and(|s| {
            s.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
        }) {
            error(&mut stream, 415, "Content-Type must be application/json");
            return;
        }
        let message: Value = match serde_json::from_slice(&r.body) {
            Ok(v) => v,
            Err(e) => {
                respond(&mut stream,400,"application/json",json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}}).to_string().as_bytes(),&[]);
                return;
            }
        };
        if !message.is_object() || message["jsonrpc"] != "2.0" || !message["method"].is_string() {
            respond(&mut stream,400,"application/json",json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}).to_string().as_bytes(),&[]);
            return;
        }
        if !self.options.stateless {
            if message["method"] == "initialize" {
                if session_id.is_some() {
                    error(&mut stream, 400, "Session already initialized");
                    return;
                }
                let mut random = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut random);
                let id = random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                sessions.insert(
                    id.clone(),
                    Session {
                        last: Instant::now(),
                    },
                );
                session_id = Some(id);
            } else {
                let Some(id) = &session_id else {
                    error(&mut stream, 400, "Missing Mcp-Session-Id header");
                    return;
                };
                let Some(session) = sessions.get_mut(id) else {
                    error(&mut stream, 404, "Session not found");
                    return;
                };
                session.last = Instant::now();
            }
        }
        drop(sessions);
        let mut headers = vec![];
        if let Some(id) = session_id.filter(|_| !self.options.stateless) {
            headers.push(("Mcp-Session-Id".into(), id));
        }
        if let Some(reply) = self.server.rpc(&message) {
            let body = match crate::protocol::encode_json(&reply) {
                Ok(body) => body,
                Err(_) => {
                    error(&mut stream, 500, "MCP response exceeds the byte limit");
                    return;
                }
            };
            if self.options.json_response {
                respond(&mut stream, 200, "application/json", &body, &headers);
            } else {
                headers.push(("Cache-Control".into(), "no-cache".into()));
                let mut event = Vec::with_capacity(body.len() + 24);
                event.extend_from_slice(b"event: message\ndata: ");
                event.extend_from_slice(&body);
                event.extend_from_slice(b"\n\n");
                respond(&mut stream, 200, "text/event-stream", &event, &headers);
            }
        } else {
            respond(&mut stream, 202, "application/json", b"", &headers);
        }
    }
}
/// An accepted connection, in blocking mode: Windows sockets inherit the
/// listener's non-blocking mode, Unix ones are returned unchanged.
fn accepted(stream: TcpStream) -> TcpStream {
    #[cfg(windows)]
    let _ = stream.set_nonblocking(false);
    stream
}
pub fn run(listener: TcpListener, server: Arc<HttpServer>, stop: &AtomicBool) -> Result<()> {
    listener.set_nonblocking(true).map_err(crate::Error::from)?;
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let mut stream = accepted(stream);
                if active.load(Ordering::Relaxed) >= 64 {
                    respond(
                        &mut stream,
                        503,
                        "application/json",
                        b"{\"error\":\"too many connections\"}",
                        &[],
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let active = active.clone();
                let server = server.clone();
                std::thread::spawn(move || {
                    struct Guard(Arc<AtomicUsize>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    let _guard = Guard(active);
                    server.handle(stream);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_accept_tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    /// A request that arrives after the accept is still read: the accepted
    /// socket blocks instead of failing with WouldBlock.
    #[test]
    fn accepted_connections_block_on_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            client.write_all(b"late").unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => panic!("{e}"),
            }
        };
        let mut stream = super::accepted(stream);
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"late");
        writer.join().unwrap();
    }
}

#[cfg(test)]
mod tests {
    /// Graphify 0.9.94 (#4656): an IPv6 bind accepts its own bracketed Host
    /// header, with or without the port, and still refuses a foreign one.
    #[test]
    fn an_ipv6_bind_accepts_its_bracketed_host_header() {
        let options = super::HttpOptions {
            host: "::1".into(),
            ..Default::default()
        };
        for header in ["[::1]:8080", "[::1]", "localhost:8080", "127.0.0.1"] {
            assert!(super::host_allowed(header, &options), "{header}");
        }
        for header in ["evil.example:8080", "::1", "::1:8080", "[localhost]:8080"] {
            assert!(!super::host_allowed(header, &options), "{header}");
        }
        let v4 = super::HttpOptions::default();
        assert!(super::host_allowed("127.0.0.1:8080", &v4));
        assert!(!super::host_allowed("[127.0.0.1]:8080", &v4));
    }
}
