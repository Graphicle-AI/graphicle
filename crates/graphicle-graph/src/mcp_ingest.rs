// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/mcp_ingest.py
//! MCP configuration indexing, translated from graphify/mcp_ingest.py.
use crate::{
    extractors::base::{file_stem, make_id},
    fsutil, io_compat, regex_compat, security,
};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, fs::File, io::Read, path::Path, sync::LazyLock};

pub const MCP_CONFIG_FILENAMES: &[&str] = &[
    ".mcp.json",
    "claude_desktop_config.json",
    "mcp.json",
    "mcp_servers.json",
];
pub const MAX_BYTES: u64 = 1_048_576;
pub const MAX_SERVERS_PER_FILE: usize = 200;
pub fn is_mcp_config_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| MCP_CONFIG_FILENAMES.contains(&s))
}
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
static NPM: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex_compat::compile(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:@[\w.\-+]+)?$")
});
static PY_MCP: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex_compat::compile(
        r"^[a-z0-9][a-z0-9._-]*-mcp(?:-[a-z0-9._-]+)?$|^mcp-[a-z0-9][a-z0-9._-]*$",
    )
});
static FLAG: LazyLock<regex::Regex> = LazyLock::new(|| regex_compat::compile(r"^-{1,2}\w"));
pub fn strip_version(package: &str) -> &str {
    let start = usize::from(package.starts_with('@'));
    package[start..]
        .find('@')
        .map_or(package, |at| &package[..start + at])
}
pub fn detect_package_from_args(args: &[Value]) -> Option<String> {
    for raw in args {
        let Some(raw) = raw.as_str() else {
            continue;
        };
        let arg = trim(raw);
        if arg.is_empty() || FLAG.is_match(arg) {
            continue;
        }
        if NPM.is_match(arg) {
            return Some(strip_version(arg).into());
        }
        if PY_MCP.is_match(arg) {
            return Some(arg.into());
        }
    }
    None
}
fn error(reason: impl Into<String>) -> Value {
    json!({"nodes":[],"edges":[],"error":reason.into()})
}
fn utf8_error(bytes: &[u8], e: std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let len = e.error_len().unwrap_or(bytes.len() - pos);
    let where_ = if len == 1 {
        format!("byte 0x{:02x} in position {pos}", bytes[pos])
    } else {
        format!("bytes in position {pos}-{}", pos + len - 1)
    };
    let reason = if e.error_len().is_none() {
        "unexpected end of data"
    } else if !(0xc2..=0xf4).contains(&bytes[pos]) {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    };
    format!("'utf-8' codec can't decode {where_}: {reason}")
}
struct Output<'a> {
    path: &'a str,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
}
impl Output<'_> {
    fn node(&mut self, id: &str, label: &str, kind: &str, line: usize) {
        if id.is_empty() || !self.seen_nodes.insert(id.into()) {
            return;
        }
        self.nodes.push(json!({"id":id,"label":security::sanitize_label(&json!(label)),"file_type":"code","source_file":self.path,
            "source_location":format!("L{line}"),"metadata":{"mcp_kind":kind}}));
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self
                .seen_edges
                .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","confidence_score":1.0,
            "source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn emit_server(&mut self, name: &str, spec: &Map<String, Value>, file_id: &str, stem: &str) {
        let id = make_id([stem, "mcp_server", name]);
        self.node(&id, name, "mcp_server", 1);
        self.edge(file_id, &id, "contains", 1, None);
        if let Some(command) = spec
            .get("command")
            .and_then(Value::as_str)
            .map(trim)
            .filter(|s| !s.is_empty())
        {
            let target = make_id(["mcp_command", command]);
            self.node(&target, command, "mcp_command", 1);
            self.edge(&id, &target, "references", 1, Some("command"));
        }
        if let Some(package) = spec
            .get("args")
            .and_then(Value::as_array)
            .and_then(|args| detect_package_from_args(args))
        {
            let target = make_id(["mcp_package", &package]);
            self.node(&target, &package, "mcp_package", 1);
            self.edge(&id, &target, "references", 1, Some("package"));
        }
        if let Some(env) = spec.get("env").and_then(Value::as_object) {
            for name in env.keys().filter(|n| !n.is_empty()) {
                let target = make_id(["env_var", name]);
                self.node(&target, name, "env_var", 1);
                self.edge(&id, &target, "requires_env", 1, None);
            }
        }
    }
}
pub fn extract_mcp_config(path: &Path) -> Value {
    let mut raw = vec![];
    if let Err(e) = File::open(path).and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut raw)) {
        return error(format!(
            "mcp_ingest read error: {}",
            io_compat::read_error(&e, path)
        ));
    }
    if raw.len() as u64 > MAX_BYTES {
        return error("mcp config too large to index");
    }
    let text = match std::str::from_utf8(&raw) {
        Ok(text) => text,
        Err(e) => return error(format!("mcp_ingest decode error: {}", utf8_error(&raw, e))),
    };
    let doc: Value = match serde_json::from_str(text) {
        Ok(doc) => doc,
        Err(e) => return error(format!("mcp_ingest json error: {e}")),
    };
    let Some(doc) = doc.as_object() else {
        return error("mcp_ingest: root is not an object");
    };
    let servers = doc
        .get("mcpServers")
        .and_then(Value::as_object)
        .or_else(|| {
            doc.get("mcp")
                .and_then(Value::as_object)
                .and_then(|m| m.get("servers"))
                .and_then(Value::as_object)
        });
    let Some(servers) = servers else {
        return error("mcp_ingest: no mcpServers map");
    };
    let source_file = fsutil::path_string(path);
    let file_id = make_id([source_file.as_str()]);
    let stem = file_stem(path);
    let mut output = Output {
        path: &source_file,
        nodes: vec![],
        edges: vec![],
        seen_nodes: HashSet::new(),
        seen_edges: HashSet::new(),
    };
    output.node(
        &file_id,
        &path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default(),
        "mcp_config_file",
        1,
    );
    for (name, spec) in servers
        .iter()
        .filter(|(name, spec)| !name.is_empty() && spec.is_object())
        .take(MAX_SERVERS_PER_FILE)
    {
        output.emit_server(name, spec.as_object().unwrap(), &file_id, &stem);
    }
    json!({"nodes":output.nodes,"edges":output.edges})
}
