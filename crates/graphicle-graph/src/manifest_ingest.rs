// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/manifest_ingest.py
//! Deterministic package-manifest ingestion from graphify/manifest_ingest.py.
use crate::{
    Result, extractors::yaml_frontmatter, fsutil, ids, io_compat, python_compat, regex_compat,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};

pub const PACKAGE_MANIFEST_NAMES: &[(&str, &str)] = &[
    ("apm.yml", "apm"),
    ("apm.yaml", "apm"),
    ("pyproject.toml", "python"),
    ("cargo.toml", "cargo"),
    ("go.mod", "go"),
    ("pom.xml", "maven"),
];
pub const MAX_MANIFEST_BYTES: u64 = 2_000_000;
pub fn ecosystem(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    PACKAGE_MANIFEST_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, eco)| *eco)
}
pub fn is_package_manifest_path(path: &Path) -> bool {
    ecosystem(path).is_some()
}
pub fn pkg_id(name: &str) -> String {
    ids::make_id(["pkg", name])
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn compile(pattern: &str) -> regex::Regex {
    let space = fsutil::PY_WHITESPACE;
    let nonspace = format!("[^{}]", &space[1..space.len() - 1]);
    regex_compat::compile(&pattern.replace(r"\S", &nonspace))
}
struct Patterns {
    name: regex::Regex,
    version: regex::Regex,
    deps: regex::Regex,
    list_dep: regex::Regex,
    map_dep: regex::Regex,
    top: regex::Regex,
    pep: regex::Regex,
    module: regex::Regex,
    require_block: regex::Regex,
    block_dep: regex::Regex,
    require: regex::Regex,
    namespace: regex::Regex,
}
static P: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    name: compile(r#"^name:\s*["']?([^"'\s#]+)"#),
    version: compile(r#"^version:\s*["']?([^"'\s#]+)"#),
    deps: compile(r"^dependencies:\s*$"),
    list_dep: compile(r#"^\s*-\s*["']?([^"'\s#:]+)"#),
    map_dep: compile(r"^\s{2,}([A-Za-z0-9._/@-]+)\s*:"),
    top: compile(r"^\S"),
    pep: compile(r"[\s<>=!~;\[\(]"),
    module: compile(r"^module\s+(\S+)"),
    require_block: compile(r"^require\s*\("),
    block_dep: compile(r"^(\S+)\s+v\S+"),
    require: compile(r"^require\s+(\S+)\s+v\S+"),
    namespace: compile(r#"\sxmlns="[^"]*""#),
});
pub fn coerce_deps(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.keys().next().cloned(),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}
pub fn parse_apm(text: &str) -> Result<Value> {
    let data = yaml_frontmatter::parse(text)?;
    if !data.is_object() {
        return Ok(Value::Null);
    }
    Ok(
        json!({"name":data["name"],"version":data["version"],"deps":coerce_deps(&data["dependencies"])}),
    )
}
pub fn parse_apm_fallback(text: &str) -> Value {
    let (mut name, mut version) = (None::<String>, None::<String>);
    let mut deps = vec![];
    let mut in_deps = false;
    for line in fsutil::split_lines(text) {
        if !in_deps {
            if let Some(c) = P.name.captures(line) {
                name = Some(c[1].into());
                continue;
            }
            if let Some(c) = P.version.captures(line) {
                version = Some(c[1].into());
                continue;
            }
        }
        if P.deps.is_match(line) {
            in_deps = true;
            continue;
        }
        if in_deps {
            if let Some(c) = P
                .list_dep
                .captures(line)
                .or_else(|| P.map_dep.captures(line))
            {
                deps.push(c[1].to_string());
            } else if P.top.is_match(line) {
                in_deps = false;
            }
        }
    }
    match name.filter(|s| !s.is_empty()) {
        Some(name) => json!({"name":name,"version":version,"deps":deps}),
        None => Value::Null,
    }
}
pub fn pep508_name(spec: &str) -> String {
    P.pep.splitn(trim(spec), 2).next().unwrap_or("").into()
}
fn load_toml(text: &str) -> Result<Value> {
    let data: toml::Value = toml::from_str(text).map_err(|e: toml::de::Error| e.to_string())?;
    serde_json::to_value(data).map_err(crate::Error::from)
}
fn iter_values(value: &Value) -> Result<Vec<Value>> {
    match value {
        Value::Object(o) => Ok(o.keys().map(|k| json!(k)).collect()),
        Value::Array(a) => Ok(a.clone()),
        Value::String(s) => Ok(s.chars().map(|c| json!(c.to_string())).collect()),
        _ if !truth(value) => Ok(vec![]),
        _ => Err(format!(
            "'{}' object is not iterable",
            python_compat::type_name(value)
        )
        .into()),
    }
}
pub fn parse_pyproject(text: &str) -> Result<Value> {
    let data = load_toml(text)?;
    let project = data
        .get("project")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let poetry = data
        .get("tool")
        .filter(|t| t.is_object())
        .and_then(|t| t.get("poetry"))
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let name = if truth(&project["name"]) {
        &project["name"]
    } else {
        &poetry["name"]
    };
    if !truth(name) {
        return Ok(Value::Null);
    }
    let mut deps = iter_values(&project["dependencies"])?
        .iter()
        .filter_map(Value::as_str)
        .map(pep508_name)
        .collect::<Vec<_>>();
    for dep in iter_values(&poetry["dependencies"])? {
        let dep = python_compat::string(&dep);
        if dep.to_lowercase() != "python" {
            deps.push(dep);
        }
    }
    let version = if truth(&project["version"]) {
        &project["version"]
    } else {
        &poetry["version"]
    };
    Ok(json!({"name":name,"version":version,"deps":deps}))
}
pub fn parse_cargo(text: &str) -> Result<Value> {
    let data = load_toml(text)?;
    let Some(name) = data
        .get("package")
        .and_then(Value::as_object)
        .and_then(|package| package.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        // A virtual workspace root declares no package of its own; it is
        // skipped by design rather than reported as an empty source (#3910).
        if data.get("workspace").is_some_and(Value::is_object) {
            return Ok(json!({"name":null,"skipped":"virtual workspace root Cargo.toml"}));
        }
        return Ok(Value::Null);
    };
    let package = &data["package"];
    let version = package.get("version").and_then(Value::as_str);
    let mut deps = coerce_deps(&data["dependencies"]);
    if let Some(targets) = data.get("target").and_then(Value::as_object) {
        for cfg in targets.values().filter(|c| c.is_object()) {
            deps.extend(coerce_deps(&cfg["dependencies"]));
        }
    }
    Ok(json!({"name":name,"version":version,"deps":deps}))
}
pub fn parse_gomod(text: &str) -> Value {
    let mut name = None::<String>;
    let mut deps = vec![];
    let mut in_block = false;
    for line in fsutil::split_lines(text) {
        let s = trim(line);
        if name.is_none()
            && let Some(c) = P.module.captures(s)
        {
            name = Some(c[1].into());
            continue;
        }
        if P.require_block.is_match(s) {
            in_block = true;
            continue;
        }
        if in_block {
            if s.starts_with(')') {
                in_block = false;
                continue;
            }
            if let Some(c) = P.block_dep.captures(s) {
                deps.push(c[1].to_string());
            }
        } else if let Some(c) = P.require.captures(s) {
            deps.push(c[1].to_string());
        }
    }
    match name.filter(|s| !s.is_empty()) {
        Some(name) => json!({"name":name,"version":null,"deps":deps}),
        None => Value::Null,
    }
}
fn xml_tag(node: roxmltree::Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name && node.tag_name().namespace().is_none()
}
fn xml_findtext(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    let child = node.children().find(|c| xml_tag(*c, name))?;
    // ElementTree omits comments/PIs and returns only text before the first element.
    Some(
        child
            .children()
            .take_while(|c| !c.is_element())
            .filter(|c| c.is_text())
            .filter_map(|c| c.text())
            .collect(),
    )
}
/// ElementTree `findtext` over a `/`-separated child path: the text of the
/// first element in document order that the path selects.
fn xml_findpath(node: roxmltree::Node<'_, '_>, path: &[&str]) -> Option<String> {
    match path {
        [] => None,
        [last] => xml_findtext(node, last),
        [first, rest @ ..] => node
            .children()
            .filter(|c| xml_tag(*c, first))
            .find_map(|c| xml_findpath(c, rest)),
    }
}
/// ElementTree `findall` over a `/`-separated child path, in document order.
fn xml_findall<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    path: &[&str],
) -> Vec<roxmltree::Node<'a, 'i>> {
    let mut level = vec![node];
    for name in path {
        level = level
            .into_iter()
            .flat_map(|n| n.children().filter(|c| xml_tag(*c, name)))
            .collect();
    }
    level
}
fn py_strip(value: &str) -> String {
    value
        .trim_matches(crate::dedup_strings::whitespace)
        .to_string()
}
/// Python `a or b` over optional element text.
fn text_or(a: Option<String>, b: Option<String>) -> Option<String> {
    if a.as_deref().is_some_and(|s| !s.is_empty()) {
        a
    } else {
        b
    }
}
static POM_PLACEHOLDER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$\{([^}]+)\}").unwrap());
pub fn parse_pom(text: &str) -> Result<Value> {
    let text = P.namespace.replacen(text, 1, "");
    let doc = roxmltree::Document::parse_with_options(
        &text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(crate::Error::from)?;
    let root = doc.root_element();
    let gid = text_or(
        xml_findtext(root, "groupId"),
        xml_findpath(root, &["parent", "groupId"]),
    );
    let version = text_or(
        xml_findtext(root, "version"),
        xml_findpath(root, &["parent", "version"]),
    );
    let Some(aid) = xml_findtext(root, "artifactId").filter(|s| !s.is_empty()) else {
        return Ok(Value::Null);
    };
    // #3806: `${property}` / `${project.*}` placeholders resolve offline, one
    // level, from <properties> and the project/parent coordinates.
    let mut props = HashMap::<String, String>::new();
    for block in root.children().filter(|c| xml_tag(*c, "properties")) {
        for prop in block.children().filter(roxmltree::Node::is_element) {
            let text: String = prop
                .children()
                .take_while(|c| !c.is_element())
                .filter_map(|c| c.is_text().then(|| c.text()).flatten())
                .collect();
            if !text.is_empty() {
                let tag = prop.tag_name();
                let key = match tag.namespace() {
                    Some(ns) => format!("{{{ns}}}{}", tag.name()),
                    None => tag.name().to_string(),
                };
                props.insert(key, py_strip(&text));
            }
        }
    }
    for (key, value) in [
        ("project.groupId", gid.clone()),
        ("project.artifactId", Some(aid.clone())),
        ("project.version", version.clone()),
        (
            "project.parent.groupId",
            xml_findpath(root, &["parent", "groupId"]),
        ),
        (
            "project.parent.version",
            xml_findpath(root, &["parent", "version"]),
        ),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            props.entry(key.into()).or_insert_with(|| py_strip(&value));
        }
    }
    let resolve = |value: Option<String>| -> Option<String> {
        match value {
            Some(v) if !v.is_empty() => Some(
                POM_PLACEHOLDER
                    .replace_all(&py_strip(&v), |c: &regex::Captures<'_>| {
                        props
                            .get(&c[1])
                            .cloned()
                            .unwrap_or_else(|| c[0].to_string())
                    })
                    .into_owned(),
            ),
            other => other,
        }
    };
    let gid = resolve(gid);
    let version = resolve(version);
    let name = match gid.filter(|s| !s.is_empty()) {
        Some(gid) => format!("{gid}:{aid}"),
        None => aid,
    };
    let mut deps = vec![];
    // Only the project's own <dependencies> and those of its profiles
    // (Graphify 0.9.94, #4596): <dependencyManagement> only pins versions, and
    // a plugin's <dependencies> belong to the build tool.
    let own = xml_findall(root, &["dependencies", "dependency"]);
    let profiles = xml_findall(root, &["profiles", "profile", "dependencies", "dependency"]);
    for dep in own.into_iter().chain(profiles) {
        if let Some(da) = resolve(xml_findtext(dep, "artifactId")).filter(|s| !s.is_empty()) {
            deps.push(
                match resolve(xml_findtext(dep, "groupId")).filter(|s| !s.is_empty()) {
                    Some(dg) => format!("{dg}:{da}"),
                    None => da,
                },
            );
        }
    }
    Ok(json!({"name":name,"version":version,"deps":deps}))
}
pub fn parse_manifest(ecosystem: &str, text: &str) -> Result<Value> {
    match ecosystem {
        "apm" => parse_apm(text),
        "python" => parse_pyproject(text),
        "cargo" => parse_cargo(text),
        "go" => Ok(parse_gomod(text)),
        "maven" => parse_pom(text),
        "apm_fallback" => Ok(parse_apm_fallback(text)),
        _ => Err(format!("Unknown manifest ecosystem: {ecosystem}").into()),
    }
}
fn error(reason: impl Into<String>) -> Value {
    json!({"nodes":[],"edges":[],"error":reason.into()})
}
pub fn extract_package_manifest(path: &Path) -> Result<Value> {
    let metadata = match path.metadata() {
        Ok(m) => m,
        Err(e) => {
            return Ok(error(format!(
                "manifest read error: {}",
                io_compat::read_error(&e, path)
            )));
        }
    };
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Ok(error("manifest too large to index"));
    }
    let text = match fsutil::read_text_lossy(path) {
        Ok(t) => t,
        Err(e) => {
            return Ok(error(format!(
                "manifest read error: {}",
                io_compat::read_error(&e, path)
            )));
        }
    };
    let ecosystem = ecosystem(path)
        .ok_or_else(|| format!("Unsupported manifest filename: {}", path.display()))?;
    let info = match parse_manifest(ecosystem, &text) {
        Ok(info) => info,
        Err(e) => return Ok(error(format!("manifest parse error: {e}"))),
    };
    if truth(&info["skipped"]) {
        return Ok(json!({"nodes":[],"edges":[],"skipped":info["skipped"].clone()}));
    }
    if !truth(&info) || !truth(&info["name"]) {
        return Ok(json!({"nodes":[],"edges":[]}));
    }
    let name = info["name"]
        .as_str()
        .ok_or("Package name must be a string")?;
    let path = fsutil::path_string(path);
    let id = pkg_id(name);
    let mut node = json!({"id":id,"label":name,"file_type":"code","type":"package","ecosystem":ecosystem,"source_file":path,"source_location":"L1"});
    if truth(&info["version"]) {
        node["version"] = info["version"].clone();
    }
    let mut edges = vec![];
    let mut seen = HashSet::new();
    for dep in info["deps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let target = pkg_id(dep);
        if target == id || !seen.insert(target.clone()) {
            continue;
        }
        edges.push(json!({"source":id,"target":target,"relation":"depends_on","context":"dependency","confidence":"EXTRACTED","confidence_score":1.0,"source_file":path,"source_location":"L1","weight":1.0}));
    }
    Ok(json!({"nodes":[node],"edges":edges}))
}
