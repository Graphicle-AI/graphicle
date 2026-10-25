// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Native project XML and XAML extraction, including code-behind and MVVM links.

#[derive(Clone, Copy)]
pub struct ProjectEdgeEvidence<'a> {
    pub line: Option<usize>,
    pub context: Option<&'a str>,
    pub confidence: &'a str,
    pub dedup: bool,
}
use crate::{
    Result, extract_languages,
    extract_merge::CONSTANTS,
    extractors::{
        base::{file_stem, make_id, read_source_text},
        resolution::{resolved, s},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};
fn read(path: &Path, kind: &str) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|_| format!("cannot read {}", path.display()))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(format!("{kind} file too large").into());
    }
    let source = String::from_utf8_lossy(&bytes).into_owned();
    if !xml_is_safe(&bytes) {
        return Err("refusing XML with DOCTYPE/ENTITY declaration".into());
    }
    Ok(source)
}
pub fn xml_is_safe(src: &[u8]) -> bool {
    let src = String::from_utf8_lossy(src).to_ascii_lowercase();
    !src.contains("<!doctype") && !src.contains("<!entity")
}
struct Output<'a> {
    path: &'a Path,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    ids: BTreeSet<String>,
    edge_keys: BTreeSet<(String, String, String, String)>,
}
impl<'a> Output<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            nodes: vec![],
            edges: vec![],
            ids: BTreeSet::new(),
            edge_keys: BTreeSet::new(),
        }
    }
    fn node(
        &mut self,
        id: &str,
        label: &str,
        line: Option<usize>,
        kind: &str,
        source: Option<&Path>,
    ) {
        if self.ids.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":kind,"source_file":source.unwrap_or(self.path),"source_location":line.map(|l|format!("L{l}"))}));
        }
    }
    fn existing_node(&mut self, n: &Value) {
        if !s(n, "id").is_empty() && self.ids.insert(s(n, "id").into()) {
            self.nodes.push(n.clone());
        }
    }
    fn edge(&mut self, src: &str, tgt: &str, rel: &str, context: ProjectEdgeEvidence<'_>) {
        let ProjectEdgeEvidence {
            line,
            context,
            confidence,
            dedup,
        } = context;

        let key = (
            src.into(),
            tgt.into(),
            rel.into(),
            context.unwrap_or("").into(),
        );
        if dedup && !self.edge_keys.insert(key) {
            return;
        }
        let mut e = json!({"source":src,"target":tgt,"relation":rel,"confidence":confidence,"source_file":self.path,"weight":1.0});
        if let Some(l) = line {
            e["source_location"] = json!(format!("L{l}"));
        }
        if let Some(ctx) = context {
            e["context"] = json!(ctx);
        }
        self.edges.push(e);
    }
    fn existing_edge(&mut self, e: &Value) {
        if self.edge_keys.insert((
            s(e, "source").into(),
            s(e, "target").into(),
            s(e, "relation").into(),
            s(e, "context").into(),
        )) {
            self.edges.push(e.clone());
        }
    }
    fn finish(self) -> Value {
        json!({"nodes":self.nodes,"edges":self.edges})
    }
}
pub fn extract_project(path: &Path, kind: &str, root: Option<&Path>) -> Value {
    match project(path, kind, root) {
        Ok(v) => v,
        Err(e) => json!({"nodes":[],"edges":[],"error":e}),
    }
}
fn project(path: &Path, kind: &str, root: Option<&Path>) -> Result<Value> {
    if kind == "xaml" {
        return xaml(path, root);
    }
    let src = read(
        path,
        if kind == "lazarus_package" {
            "package"
        } else {
            "project"
        },
    )?;
    let doc = roxmltree::Document::parse(&src).map_err(|e| format!("XML parse error: {e}"))?;
    let top = doc.root_element();
    let ns = top.tag_name().namespace();
    let file = make_id([path.to_string_lossy().as_ref()]);
    let stem = file_stem(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut out = Output::new(path);
    let find = |tag| {
        top.descendants()
            .filter(move |n| {
                n.is_element() && n.tag_name().name() == tag && n.tag_name().namespace() == ns
            })
            .collect::<Vec<_>>()
    };
    out.node(
        &file,
        &name,
        if kind == "lazarus_package" {
            Some(1)
        } else {
            None
        },
        "code",
        None,
    );
    if kind == "lazarus_package" {
        let pkg = top
            .descendants()
            .find(|n| n.has_tag_name("Package"))
            .and_then(|p| p.children().find(|n| n.has_tag_name("Name")))
            .and_then(|n| n.attribute("Value"))
            .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        let id = make_id([stem.as_str(), pkg]);
        out.node(&id, pkg, Some(1), "code", None);
        out.edge(
            &file,
            &id,
            "contains",
            ProjectEdgeEvidence {
                line: Some(1),
                context: None,
                confidence: "EXTRACTED",
                dedup: false,
            },
        );
        for group in top
            .descendants()
            .filter(|n| n.has_tag_name("RequiredPkgs") || n.has_tag_name("Files"))
        {
            let dep = group.has_tag_name("RequiredPkgs");
            for item in group.children().filter(|n| n.is_element()) {
                if let Some(name) = item
                    .children()
                    .find(|n| n.has_tag_name(if dep { "PackageName" } else { "UnitName" }))
                    .and_then(|n| n.attribute("Value"))
                    .filter(|s| !s.is_empty())
                {
                    let target = if dep {
                        make_id([name])
                    } else {
                        crate::extractors::pascal_paths::resolve_unit(path, name)
                    };
                    out.node(&target, name, Some(1), "code", None);
                    out.edge(
                        &id,
                        &target,
                        if dep { "imports" } else { "contains" },
                        ProjectEdgeEvidence {
                            line: Some(1),
                            context: if dep { Some("import") } else { None },
                            confidence: "EXTRACTED",
                            dedup: false,
                        },
                    );
                }
            }
        }
        let mut result = out.finish();
        result["input_tokens"] = json!(0);
        result["output_tokens"] = json!(0);
        return Ok(result);
    }
    let resolve = |s: &str| {
        resolved(
            &path
                .parent()
                .unwrap_or(Path::new("."))
                .join(s.replace('\\', "/")),
        )
    };
    if kind == "slnx" {
        let mut projects = BTreeSet::new();
        for n in find("Project") {
            if let Some(p) = n.attribute("Path").filter(|s| !s.is_empty()) {
                let full = resolve(p);
                let id = make_id([full.to_string_lossy().as_ref()]);
                if !id.is_empty() && !out.ids.contains(&id) {
                    out.node(
                        &id,
                        &Path::new(p)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        None,
                        "code",
                        Some(&full),
                    );
                    out.edge(
                        &file,
                        &id,
                        "contains",
                        ProjectEdgeEvidence {
                            line: None,
                            context: None,
                            confidence: "EXTRACTED",
                            dedup: false,
                        },
                    );
                }
                if !id.is_empty() {
                    projects.insert(id);
                }
            }
        }
        for n in find("Project") {
            if let Some(p) = n.attribute("Path").filter(|s| !s.is_empty()) {
                let from = make_id([resolve(p).to_string_lossy().as_ref()]);
                for d in n.descendants().filter(|n| {
                    n.is_element()
                        && n.tag_name().name() == "BuildDependency"
                        && n.tag_name().namespace() == ns
                }) {
                    if let Some(p) = d.attribute("Project").filter(|s| !s.is_empty()) {
                        let to = make_id([resolve(p).to_string_lossy().as_ref()]);
                        if !from.is_empty()
                            && !to.is_empty()
                            && from != to
                            && projects.contains(&to)
                        {
                            out.edge(
                                &from,
                                &to,
                                "imports",
                                ProjectEdgeEvidence {
                                    line: None,
                                    context: None,
                                    confidence: "EXTRACTED",
                                    dedup: false,
                                },
                            );
                        }
                    }
                }
            }
        }
        return Ok(out.finish());
    }
    for tag in ["TargetFramework", "TargetFrameworks"] {
        for n in find(tag) {
            if let Some(text) = n.text() {
                for fw in if tag == "TargetFrameworks" {
                    text.trim().split(';').map(str::trim).collect::<Vec<_>>()
                } else {
                    vec![text.trim()]
                } {
                    if fw.is_empty() && tag == "TargetFrameworks" {
                        continue;
                    }
                    let id = make_id(["framework", fw]);
                    if !id.is_empty() && !out.ids.contains(&id) {
                        out.node(&id, fw, None, "concept", None);
                        out.edge(
                            &file,
                            &id,
                            "references",
                            ProjectEdgeEvidence {
                                line: None,
                                context: None,
                                confidence: "EXTRACTED",
                                dedup: false,
                            },
                        );
                    }
                }
            }
        }
    }
    for n in find("PackageReference") {
        let name = n
            .attribute("Include")
            .or_else(|| n.attribute("include"))
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let version = n
            .attribute("Version")
            .or_else(|| n.attribute("version"))
            .unwrap_or("");
        let id = make_id(["nuget", name]);
        let label = if version.is_empty() {
            name.into()
        } else {
            format!("{name} ({version})")
        };
        if !id.is_empty() {
            out.node(&id, &label, None, "code", None);
        }
        out.edge(
            &file,
            &id,
            "imports",
            ProjectEdgeEvidence {
                line: None,
                context: None,
                confidence: "EXTRACTED",
                dedup: false,
            },
        );
    }
    for n in find("ProjectReference") {
        let name = n
            .attribute("Include")
            .or_else(|| n.attribute("include"))
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let name = name.replace('\\', "/");
        let full = resolve(&name);
        let id = make_id([full.to_string_lossy().as_ref()]);
        if !id.is_empty() {
            out.node(
                &id,
                &Path::new(&name)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                None,
                "code",
                Some(&full),
            );
        }
        out.edge(
            &file,
            &id,
            "imports",
            ProjectEdgeEvidence {
                line: None,
                context: None,
                confidence: "EXTRACTED",
                dedup: false,
            },
        );
    }
    if let Some(sdk) = top.attribute("Sdk").filter(|s| !s.is_empty()) {
        let id = make_id(["sdk", sdk]);
        if !id.is_empty() && !out.ids.contains(&id) {
            out.node(&id, sdk, None, "concept", None);
            out.edge(
                &file,
                &id,
                "references",
                ProjectEdgeEvidence {
                    line: None,
                    context: None,
                    confidence: "EXTRACTED",
                    dedup: false,
                },
            );
        }
    }
    Ok(out.finish())
}
pub fn markup(value: &str) -> Option<(String, String)> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() || inner.starts_with('}') {
        return None;
    }
    let (name, args) = inner.split_once(' ').unwrap_or((inner, ""));
    Some((name.into(), args.trim().into()))
}
pub fn split_markup(args: &str) -> Vec<String> {
    let (mut parts, mut start, mut depth) = (vec![], 0, 0);
    for (i, c) in args.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                parts.push(args[start..i].trim().into());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.into());
    }
    parts
}
pub fn static_resource(value: &str) -> Option<String> {
    let (name, args) = markup(value)?;
    if name != "StaticResource" {
        return None;
    }
    for part in split_markup(&args) {
        let resource = if let Some((k, v)) = part.split_once('=') {
            if k.trim() != "ResourceKey" {
                continue;
            }
            v.trim()
        } else {
            part.trim()
        };
        return (!resource.is_empty()).then_some(resource.into());
    }
    None
}
pub fn binding_refs(value: &str) -> (Option<String>, Option<String>) {
    let Some((name, args)) = markup(value) else {
        return (None, None);
    };
    if name != "Binding" {
        return (None, None);
    }
    let (mut path, mut converter) = (None, None);
    for part in split_markup(&args) {
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "Path" => path = Some(v.trim().to_string()),
                "Converter" => converter = static_resource(v.trim()),
                _ => {}
            }
        } else if path.is_none() {
            path = Some(part.trim().to_string());
        }
    }
    (
        path.filter(|p| !p.is_empty() && !p.contains(['{', '}'])),
        converter.filter(|p| !p.is_empty()),
    )
}
pub fn codebehind_path(path: &Path) -> Option<PathBuf> {
    let expected = path.with_extension(format!(
        "{}.cs",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    if expected.exists() {
        return Some(expected);
    }
    let name = expected.file_name()?.to_string_lossy().to_lowercase();
    std::fs::read_dir(path.parent().unwrap_or(Path::new(".")))
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().to_lowercase() == name)
        .map(|e| e.path())
}
pub fn codebehind_symbols(
    path: &Path,
    class: Option<&str>,
) -> (Option<Value>, BTreeMap<String, Value>, Vec<Value>) {
    let Some(cb) = codebehind_path(path) else {
        return (None, BTreeMap::new(), vec![]);
    };
    let result = extract_languages::extract_language(&cb, "CSHARP");
    if result.get("error").is_some() {
        return (None, BTreeMap::new(), vec![]);
    }
    let class = class
        .and_then(|class| {
            result["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|n| s(n, "label") == class.rsplit('.').next().unwrap_or(""))
        })
        .cloned();
    let edges = result["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| {
            class.as_ref().is_some_and(|n| e["source"] == n["id"]) && s(e, "relation") == "method"
        })
        .cloned()
        .collect::<Vec<_>>();
    let ids = edges
        .iter()
        .map(|e| s(e, "target"))
        .collect::<BTreeSet<_>>();
    let lines = read_source_text(&cb, false)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let re =
        regex::Regex::new(r"\(\s*object\??\s+\w+\s*,\s*[\w.]*EventArgs(?:<[^>]*>)?\s+\w+\s*\)")
            .unwrap();
    let mut methods = BTreeMap::new();
    for n in result["nodes"].as_array().into_iter().flatten() {
        if class.is_some() && !ids.contains(s(n, "id")) {
            continue;
        }
        let label = s(n, "label");
        let line = s(n, "source_location")
            .trim_start_matches('L')
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse::<usize>()
            .unwrap_or(0);
        if label.starts_with('.')
            && label.ends_with("()")
            && line > 0
            && line <= lines.len()
            && re.is_match(&lines[line - 1..(line + 2).min(lines.len())].join(" "))
        {
            methods.insert(
                label
                    .trim_matches(['(', ')'])
                    .trim_start_matches('.')
                    .into(),
                n.clone(),
            );
        }
    }
    (class, methods, edges)
}
fn ident(value: &str) -> bool {
    static IDENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[A-Za-z_]\w*$").unwrap());
    IDENT.is_match(value)
}
pub fn simple_type(value: &str) -> Option<String> {
    let mut v = value
        .trim()
        .trim_matches(['{', '}'])
        .split(',')
        .next()
        .unwrap_or("")
        .trim();
    v = v.strip_prefix("x:Type ").unwrap_or(v).trim();
    v = v.rsplit(':').next().unwrap_or("");
    v = v.rsplit('.').next().unwrap_or("");
    v = v.rsplit('+').next().unwrap_or("");
    ident(v).then_some(v.into())
}
pub fn inferred_viewmodels(view: &str) -> Vec<String> {
    let mut out = vec![];
    if view == "MainWindow" {
        out.extend(["MainWindowViewModel".into(), "MainViewModel".into()]);
    }
    for suffix in ["UserControl", "View", "Page", "Control"] {
        if let Some(prefix) = view.strip_suffix(suffix).filter(|s| !s.is_empty()) {
            let v = format!("{prefix}ViewModel");
            if !out.contains(&v) {
                out.push(v);
            }
            break;
        }
    }
    out
}
pub fn project_root(path: &Path, boundary: Option<&Path>) -> PathBuf {
    let mut root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    for dir in root.clone().ancestors() {
        if std::fs::read_dir(dir).is_ok_and(|es| {
            es.flatten().any(|e| {
                [".csproj", ".fsproj", ".vbproj", ".sln", ".slnx"]
                    .contains(&extract_languages::suffix(&e.path()).as_str())
            })
        }) {
            root = dir.into();
            break;
        }
    }
    if let Some(b) = boundary {
        let b = resolved(b);
        if !resolved(&root).starts_with(&b) {
            return b;
        }
    }
    root
}
pub fn csharp_viewmodels(path: &Path, boundary: Option<&Path>) -> BTreeMap<String, Vec<Value>> {
    let root = project_root(path, boundary);
    let mut warnings = crate::detect_ignore::EncodingWarnings::default();
    let patterns =
        crate::detect_ignore::load_ignore_file(&root, true, &mut warnings).unwrap_or_default();
    let mut cache = HashMap::new();
    let (mut dirs, mut files, mut count) = (vec![root.clone()], vec![], 0);
    while let Some(dir) = dirs.pop() {
        count += 1;
        if count > 20000 {
            break;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().is_ok_and(|t| t.is_dir())
                    && !name.starts_with('.')
                    && !crate::detect_types::is_noise_dir(&name, None)
                {
                    dirs.push(e.path());
                } else if name.ends_with(".cs") {
                    files.push(e.path());
                }
            }
        }
    }
    files.sort();
    let mut classes = BTreeMap::new();
    for file in files {
        if crate::detect_ignore::is_ignored(&file, &root, &patterns, &mut cache) {
            continue;
        }
        let result = extract_languages::extract_language(&file, "CSHARP");
        if result.get("error").is_some() {
            continue;
        }
        for n in result["nodes"].as_array().into_iter().flatten() {
            let l = s(n, "label");
            if l.ends_with("ViewModel") && ident(l) && !s(n, "source_file").is_empty() {
                classes
                    .entry(l.into())
                    .or_insert_with(Vec::new)
                    .push(n.clone());
            }
        }
    }
    classes
}
pub fn pascal_name(name: &str) -> Option<String> {
    let n = name.trim().trim_start_matches('_');
    let n = n.strip_prefix("m_").unwrap_or(n);
    if !ident(n) {
        return None;
    }
    let mut chars = n.chars();
    Some(chars.next().unwrap().to_uppercase().collect::<String>() + chars.as_str())
}
pub fn toolkit_members(vm: &Value) -> (BTreeMap<String, Value>, Vec<Value>) {
    let (sf, id) = (s(vm, "source_file"), s(vm, "id"));
    if sf.is_empty() || id.is_empty() {
        return (BTreeMap::new(), vec![]);
    }
    let Ok(source) = read_source_text(Path::new(sf), false) else {
        return (BTreeMap::new(), vec![]);
    };
    let field = regex::Regex::new(r"\b(_?m?_?[A-Za-z_]\w*)\s*(?:=.*)?;").unwrap();
    let method = regex::Regex::new(r"\b([A-Za-z_]\w*)\s*\(").unwrap();
    let mut pending = None;
    let mut members = BTreeMap::new();
    let mut edges = vec![];
    for (i, line) in source.lines().enumerate() {
        let mut text = line;
        let remainder = line.split_once(']').map(|(_, r)| r.trim()).unwrap_or("");
        for (marker, kind) in [
            ("ObservableProperty", "property"),
            ("RelayCommand", "command"),
        ] {
            if text.contains('[') && text.contains(marker) {
                pending = Some((kind, i + 1));
                text = remainder;
            }
        }
        if pending.is_none() || text.trim().is_empty() || text.trim_start().starts_with('[') {
            continue;
        }
        let (kind, line) = pending.take().unwrap();
        let label = if kind == "property" {
            field.captures(text).and_then(|c| pascal_name(&c[1]))
        } else {
            method
                .captures(text)
                .map(|c| format!("{}Command", c[1].strip_suffix("Async").unwrap_or(&c[1])))
        };
        if let Some(label) = label {
            let nid = make_id([id, label.as_str()]);
            members.insert(label.clone(),json!({"id":nid,"label":label,"file_type":"code","source_file":sf,"source_location":format!("L{line}")}));
            edges.push(json!({"source":id,"target":nid,"relation":"defines","confidence":"INFERRED","source_file":sf,"source_location":format!("L{line}"),"weight":1.0,"context":if kind=="property"{"communitytoolkit_observable_property"}else{"communitytoolkit_relay_command"}}));
        }
    }
    (members, edges)
}
fn xaml(path: &Path, boundary: Option<&Path>) -> Result<Value> {
    let text = read(path, "xaml")?;
    let doc = roxmltree::Document::parse(&text).map_err(|e| format!("XML parse error: {e}"))?;
    let top = doc.root_element();
    let root_type = top.tag_name().name();
    let stem = file_stem(path);
    let file = make_id([path.to_string_lossy().as_ref()]);
    let root = make_id([stem.as_str(), root_type]);
    let line_for = |value: &str| {
        text.lines()
            .position(|l| !value.is_empty() && l.contains(value))
            .map(|i| i + 1)
            .unwrap_or(1)
    };
    let mut out = Output::new(path);
    out.node(
        &file,
        &path.file_name().unwrap_or_default().to_string_lossy(),
        Some(1),
        "code",
        None,
    );
    out.node(&root, root_type, Some(1), "code", None);
    out.edge(
        &file,
        &root,
        "contains",
        ProjectEdgeEvidence {
            line: Some(1),
            context: None,
            confidence: "EXTRACTED",
            dedup: true,
        },
    );
    let class_name = top
        .attributes()
        .find(|a| a.name() == "Class" && !a.value().is_empty())
        .map(|a| a.value().trim());
    let (class, methods, method_edges) = codebehind_symbols(path, class_name);
    if let Some(name) = class_name {
        let id = if let Some(c) = &class {
            out.existing_node(c);
            s(c, "id").to_string()
        } else {
            let label = name.rsplit('.').next().unwrap_or("");
            let id = make_id([stem.as_str(), label]);
            out.node(&id, label, Some(line_for(name)), "code", None);
            id
        };
        out.edge(
            &root,
            &id,
            "references",
            ProjectEdgeEvidence {
                line: Some(line_for(name)),
                context: Some("x_class"),
                confidence: "EXTRACTED",
                dedup: true,
            },
        );
    }
    let mut has_context = false;
    let mut names = vec![];
    let design = regex::Regex::new(r"\bType\s*=\s*(?:\{x:Type\s+)?([\w.:+]+)").unwrap();
    let mut autowire = false;
    for n in top.descendants().filter(|n| n.is_element()) {
        let tag = n.tag_name().name();
        if tag == "DataContext" || tag.ends_with(".DataContext") {
            has_context = true;
            for c in n.children().filter(|n| n.is_element()) {
                if let Some(name) = simple_type(c.tag_name().name())
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
        }
        for a in n.attributes() {
            if a.name().ends_with("ViewModelLocator.AutoWireViewModel")
                && a.value().trim().eq_ignore_ascii_case("true")
            {
                autowire = true;
            }
            if a.name() == "DataContext" && !a.value().is_empty() {
                has_context = true;
                if let Some(c) = design.captures(a.value())
                    && let Some(name) = simple_type(&c[1])
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
        }
    }
    let confidence = if has_context { "EXTRACTED" } else { "INFERRED" };
    if !has_context {
        let view = class_name
            .map(|s| s.rsplit('.').next().unwrap_or(""))
            .or_else(|| {
                if autowire {
                    path.file_stem().and_then(|s| s.to_str())
                } else {
                    None
                }
            })
            .unwrap_or("");
        names = inferred_viewmodels(view);
    }
    let mut generated = BTreeMap::new();
    if !names.is_empty() {
        let classes = csharp_viewmodels(path, boundary);
        let candidates = names
            .iter()
            .flat_map(|name| classes.get(name).into_iter().flatten())
            .map(|n| (s(n, "id"), n))
            .collect::<BTreeMap<_, _>>();
        if candidates.len() == 1 {
            let vm = candidates.values().next().unwrap();
            out.existing_node(vm);
            out.edge(
                &root,
                s(vm, "id"),
                "references",
                ProjectEdgeEvidence {
                    line: Some(line_for(s(vm, "label"))),
                    context: Some("view_model"),
                    confidence,
                    dedup: true,
                },
            );
            let (ms, es) = toolkit_members(vm);
            for n in ms.values() {
                out.existing_node(n);
            }
            for e in es {
                out.existing_edge(&e);
            }
            generated = ms;
        }
    }
    for n in top.descendants().filter(|n| n.is_element()) {
        let tag = n.tag_name().name();
        let name = n
            .attributes()
            .find(|a| a.name() == "Name" && !a.value().is_empty())
            .map(|a| a.value().trim());
        let owner = if let Some(name) = name {
            let id = make_id([stem.as_str(), name]);
            out.node(&id, name, Some(line_for(name)), "code", None);
            out.edge(
                &root,
                &id,
                "contains",
                ProjectEdgeEvidence {
                    line: Some(line_for(name)),
                    context: None,
                    confidence: "EXTRACTED",
                    dedup: true,
                },
            );
            let ty = make_id(["xaml", tag]);
            out.node(&ty, tag, Some(line_for(name)), "concept", None);
            out.edge(
                &id,
                &ty,
                "references",
                ProjectEdgeEvidence {
                    line: Some(line_for(name)),
                    context: Some("type"),
                    confidence: "EXTRACTED",
                    dedup: true,
                },
            );
            id
        } else {
            root.clone()
        };
        for a in n.attributes() {
            let (name, value) = (a.name(), a.value());
            if !CONSTANTS["_XAML_NON_EVENT_ATTRS"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == name)
                && ident(value)
                && let Some(m) = methods.get(value)
            {
                out.existing_node(m);
                out.edge(
                    &owner,
                    s(m, "id"),
                    "references",
                    ProjectEdgeEvidence {
                        line: Some(line_for(value)),
                        context: Some("event"),
                        confidence: "EXTRACTED",
                        dedup: true,
                    },
                );
                if let Some(e) = method_edges.iter().find(|e| e["target"] == m["id"]) {
                    if let Some(c) = &class {
                        out.existing_node(c);
                    }
                    out.existing_edge(e);
                }
            }
            let (binding, converter) = binding_refs(value);
            if let Some(b) = binding {
                let id = make_id(["binding", b.as_str()]);
                let context = if name == "Command" || name.ends_with(".Command") {
                    "binding_command"
                } else {
                    "binding_path"
                };
                out.node(&id, &b, Some(line_for(value)), "concept", None);
                out.edge(
                    &owner,
                    &id,
                    "references",
                    ProjectEdgeEvidence {
                        line: Some(line_for(value)),
                        context: Some(context),
                        confidence: "EXTRACTED",
                        dedup: true,
                    },
                );
                if let Some(m) = generated.get(&b) {
                    out.existing_node(m);
                    out.edge(
                        &owner,
                        s(m, "id"),
                        "references",
                        ProjectEdgeEvidence {
                            line: Some(line_for(value)),
                            context: Some(context),
                            confidence: "INFERRED",
                            dedup: true,
                        },
                    );
                }
            }
            if let Some(c) = converter {
                let id = make_id(["binding_converter", c.as_str()]);
                out.node(&id, &c, Some(line_for(value)), "concept", None);
                out.edge(
                    &owner,
                    &id,
                    "references",
                    ProjectEdgeEvidence {
                        line: Some(line_for(value)),
                        context: Some("binding_converter"),
                        confidence: "EXTRACTED",
                        dedup: true,
                    },
                );
            }
            if tag == "Binding" {
                let reference = if name == "Path" {
                    let p = value.trim();
                    (!p.is_empty() && !p.contains(['{', '}'])).then_some((
                        p.to_string(),
                        "binding",
                        "binding_path",
                    ))
                } else if name == "Converter" {
                    static_resource(value).map(|c| (c, "binding_converter", "binding_converter"))
                } else {
                    None
                };
                if let Some((label, prefix, context)) = reference {
                    let id = make_id([prefix, label.as_str()]);
                    out.node(&id, &label, Some(line_for(value)), "concept", None);
                    out.edge(
                        &owner,
                        &id,
                        "references",
                        ProjectEdgeEvidence {
                            line: Some(line_for(value)),
                            context: Some(context),
                            confidence: "EXTRACTED",
                            dedup: true,
                        },
                    );
                }
            }
        }
    }
    Ok(out.finish())
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let path = Path::new(r["path"].as_str().unwrap_or(""));
    let text = r["text"].as_str().unwrap_or("");
    Ok(match r["action"].as_str().unwrap_or("extract") {
        "markup" => json!(markup(text)),
        "split_markup" => json!(split_markup(text)),
        "static_resource" => json!(static_resource(text)),
        "binding_refs" => json!(binding_refs(text)),
        "codebehind_path" => json!(codebehind_path(path)),
        "codebehind_symbols" => json!(codebehind_symbols(path, r["class_name"].as_str())),
        "simple_type" => json!(simple_type(text)),
        "inferred_viewmodels" => json!(inferred_viewmodels(text)),
        "project_root" => json!(project_root(path, r["root"].as_str().map(Path::new))),
        "viewmodels" => json!(csharp_viewmodels(path, r["root"].as_str().map(Path::new))),
        "pascal_name" => json!(pascal_name(text)),
        "toolkit" => json!(toolkit_members(&r["node"])),
        "xml_safe" => json!(xml_is_safe(text.as_bytes())),
        _ => extract_project(
            path,
            r["language"].as_str().unwrap_or("xaml"),
            r["root"].as_str().map(Path::new),
        ),
    })
}
