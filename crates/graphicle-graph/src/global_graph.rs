// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/global_graph.py
//! Complete native translation of global_graph.py and its persistent store.
use crate::{
    Result, build_global, cross_repo_calls, fsutil,
    graph::Graph,
    paths,
    python_compat::{repr_string, scalar_key},
    security,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct GlobalStore {
    pub directory: PathBuf,
    pub graph_path: PathBuf,
    pub manifest_path: PathBuf,
    pub warnings: Vec<String>,
    pub graph_byte_limit: u64,
    now: Option<DateTime<Utc>>,
}
impl GlobalStore {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            graph_path: directory.join("global-graph.json"),
            manifest_path: directory.join("global-manifest.json"),
            directory,
            warnings: vec![],
            graph_byte_limit: security::max_graph_file_bytes(),
            now: None,
        }
    }
    pub fn default_location() -> Result<Self> {
        #[cfg(unix)]
        let directory = match std::env::var_os("HOME") {
            Some(value) => configured_unix_directory(&value),
            None => nix::unistd::User::from_uid(nix::unistd::Uid::current())
                .map_err(crate::Error::from)?
                .ok_or("Cannot determine the home directory")?
                .dir
                .join(paths::CONFIG_DIR),
        };
        #[cfg(windows)]
        let directory = {
            let root = std::env::var_os("USERPROFILE")
                .or_else(|| {
                    let path = std::env::var_os("HOMEPATH")?;
                    let mut drive = std::env::var_os("HOMEDRIVE").unwrap_or_default();
                    drive.push(path);
                    Some(drive)
                })
                .ok_or("Cannot determine the home directory")?;
            PathBuf::from(root).join(paths::CONFIG_DIR)
        };
        #[cfg(not(any(unix, windows)))]
        let directory =
            PathBuf::from(std::env::var_os("HOME").ok_or("Cannot determine the home directory")?)
                .join(paths::CONFIG_DIR);
        Ok(Self::new(directory))
    }
    fn now(&self) -> DateTime<Utc> {
        self.now.unwrap_or_else(Utc::now)
    }
    pub fn load_manifest(&mut self) -> Value {
        if self.manifest_path.exists() {
            match read_json(&self.manifest_path) {
                Ok(value) => return value,
                Err(error) => {
                    let backup = self.manifest_path.with_file_name(format!(
                        "{}.corrupt.{}",
                        self.manifest_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        self.now().timestamp()
                    ));
                    match fs::rename(&self.manifest_path,&backup) {
                        Ok(())=>self.warnings.push(format!("[graphicle global] manifest at {} failed to parse ({error}); moved to {} and starting fresh. Restore from the backup if this was unexpected.",fsutil::path_string(&self.manifest_path),fsutil::path_string(&backup))),
                        Err(e)=>self.warnings.push(format!("[graphicle global] manifest at {} failed to parse ({error}) and could not be backed up ({e}). Starting fresh.",fsutil::path_string(&self.manifest_path))),
                    }
                }
            }
        }
        json!({"version":1,"repos":{}})
    }
    pub fn save_manifest(&self, manifest: &Value) -> Result<()> {
        fs::create_dir_all(&self.directory).map_err(crate::Error::from)?;
        paths::write_json_atomic(&self.manifest_path, manifest, Some(2), true)
            .map_err(crate::Error::from)
    }
    pub fn load_graph(&self) -> Result<Graph> {
        if self.graph_path.exists() {
            load_graph(&self.graph_path, self.graph_byte_limit)
        } else {
            Ok(Graph::new(false, false))
        }
    }
    pub fn save_graph(&self, graph: &Graph) -> Result<()> {
        fs::create_dir_all(&self.directory).map_err(crate::Error::from)?;
        // One line, like graph.json (0.9.86, #3413).
        paths::write_json_atomic_compact(&self.graph_path, &graph.to_node_link("links"), true)
            .map_err(crate::Error::from)
    }
    pub fn add(&mut self, source: &Path, tag: &str) -> Result<Value> {
        if !source.exists() {
            return Err(format!(
                "FileNotFoundError: graph not found: {}",
                fsutil::path_string(source)
            )
            .into());
        }
        let mut manifest = self.load_manifest();
        let hash = file_hash(source)?;
        let repos = manifest
            .get("repos")
            .and_then(Value::as_object)
            .ok_or("Invalid manifest: expected repos object")?;
        let existing = repos.get(tag).cloned().unwrap_or(json!({}));
        if !existing.is_object() {
            return Err("Invalid manifest: expected repository object".into());
        }
        let resolved = fsutil::resolve_missing(source).map_err(crate::Error::from)?;
        let resolved = fsutil::path_string(&resolved);
        if let Some(old) = existing
            .get("source_path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && *s != resolved.as_str())
        {
            self.warnings.push(format!("[graphicle global] warning: repo tag '{tag}' previously pointed to {}, now updating to {}. Use --as <tag> to give it a different name.",repr_string(old),repr_string(&resolved)));
        }
        if existing.get("source_hash").and_then(Value::as_str) == Some(&hash) {
            return Ok(
                json!({"repo_tag":tag,"nodes_added":0,"nodes_removed":0,"skipped":true,"cross_repo_calls":0}),
            );
        }
        // The source keeps each link's stored direction as `_src`/`_tgt`
        // (`restore_arc_direction`, 0.9.86, #4066): an undirected load
        // re-orders endpoints by node position.
        let prefixed = build_global::prefix_graph_for_global(
            &load_graph_with_arcs(source, self.graph_byte_limit)?,
            tag,
            0,
        )?;
        let mut graph = self.load_graph()?;
        let removed = build_global::prune_repo_from_graph(&mut graph, tag)?;
        let mut external_labels = std::collections::BTreeMap::new();
        for node in &graph.nodes {
            if !truthy(node.attributes.get("source_file")) && truthy(node.attributes.get("label")) {
                let label = scalar_key(node.attributes.get("label").unwrap())
                    .ok_or("Unhashable external label")?;
                external_labels.insert(label, node.id.clone());
            }
        }
        let mut remap = std::collections::BTreeMap::new();
        for (index, node) in prefixed.nodes.iter().enumerate() {
            if !truthy(node.attributes.get("source_file")) {
                let label = scalar_key(node.attributes.get("label").unwrap_or(&Value::Null))
                    .ok_or("Unhashable external label")?;
                if let Some(existing) = external_labels.get(&label) {
                    remap.insert(index, existing.clone());
                }
            }
        }
        for (index, node) in prefixed.nodes.iter().enumerate() {
            if !remap.contains_key(&index) {
                graph.add_node(node.id.clone(), node.attributes.clone())?;
            }
        }
        // The direction markers name endpoints too: rewire them with the
        // endpoints so they never point at a deduplicated (absent) external.
        let remapped_ids = remap
            .iter()
            .filter_map(|(index, id)| Some((scalar_key(&prefixed.nodes[*index].id)?, id)))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (u, v, edge) in prefixed.iter_edges() {
            let u = remap.get(&u).unwrap_or(&prefixed.nodes[u].id);
            let v = remap.get(&v).unwrap_or(&prefixed.nodes[v].id);
            // Endpoints have already been inserted, so graph identity includes bool/int equality.
            if graph.node_index(u) != graph.node_index(v) {
                let mut attributes = edge.attributes.clone();
                for key in ["_src", "_tgt"] {
                    if let Some(new) = attributes
                        .get(key)
                        .and_then(scalar_key)
                        .and_then(|old| remapped_ids.get(&old))
                    {
                        attributes.insert(key.into(), (*new).clone());
                    }
                }
                graph.add_edge(u.clone(), v.clone(), None, attributes)?;
            }
        }
        let added = prefixed.nodes.len() - remap.len();
        let linked = cross_repo_calls::link_cross_repo_member_calls(&mut graph)?;
        self.save_graph(&graph)?;
        let now = self.now();
        let timestamp = now.to_rfc3339_opts(
            if now.timestamp_subsec_micros() == 0 {
                SecondsFormat::Secs
            } else {
                SecondsFormat::Micros
            },
            false,
        );
        manifest["repos"].as_object_mut().ok_or("Invalid repos object")?.insert(tag.into(),json!({"added_at":timestamp,"source_path":resolved,"node_count":added,"edge_count":prefixed.edge_count(),"source_hash":hash}));
        self.save_manifest(&manifest)?;
        Ok(
            json!({"repo_tag":tag,"nodes_added":added,"nodes_removed":removed,"skipped":false,"cross_repo_calls":linked}),
        )
    }
    pub fn remove(&mut self, tag: &str) -> Result<usize> {
        let mut manifest = self.load_manifest();
        let repos = manifest
            .get_mut("repos")
            .and_then(Value::as_object_mut)
            .ok_or("Invalid manifest: expected repos object")?;
        if !repos.contains_key(tag) {
            return Err(format!("KeyError: repo '{tag}' not in global graph").into());
        }
        let mut graph = self.load_graph()?;
        let removed = build_global::prune_repo_from_graph(&mut graph, tag)?;
        self.save_graph(&graph)?;
        repos.shift_remove(tag);
        self.save_manifest(&manifest)?;
        Ok(removed)
    }
    pub fn list(&mut self) -> Result<Value> {
        let manifest = self.load_manifest();
        Ok(manifest
            .as_object()
            .ok_or("Invalid manifest: expected object")?
            .get("repos")
            .cloned()
            .unwrap_or(json!({})))
    }
}
#[cfg(unix)]
fn configured_unix_directory(value: &std::ffi::OsStr) -> PathBuf {
    // posixpath.expanduser('~') treats an explicitly empty home as the root.
    if value.is_empty() {
        Path::new("/").join(paths::CONFIG_DIR)
    } else {
        PathBuf::from(value).join(paths::CONFIG_DIR)
    }
}
#[cfg(all(test, unix))]
mod home_tests {
    use super::*;
    #[test]
    fn home_expansion_preserves_empty_root_and_relative_configuration() {
        assert_eq!(
            configured_unix_directory(std::ffi::OsStr::new("")),
            PathBuf::from("/.graphicle-graph")
        );
        assert_eq!(
            configured_unix_directory(std::ffi::OsStr::new("/tmp/user/")),
            PathBuf::from("/tmp/user/.graphicle-graph")
        );
        assert_eq!(
            configured_unix_directory(std::ffi::OsStr::new("relative")),
            PathBuf::from("relative/.graphicle-graph")
        );
    }
}
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}
fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(&fs::read_to_string(path).map_err(crate::Error::from)?)
        .map_err(crate::Error::from)
}
fn load_graph(path: &Path, limit: u64) -> Result<Graph> {
    security::check_graph_file_size_cap_with(path, limit)?;
    let mut data = read_json(path)?;
    if data.get("links").is_none() && data.get("edges").is_some() {
        data["links"] = data["edges"].take();
    }
    // A graph.json written before 0.9.92 reads as a newer one does
    // (`build::mark_parallel_links`).
    crate::build::mark_parallel_links(&mut data);
    Graph::from_node_link_owned(data, "links")
}
/// [`load_graph`] keeping each link's stored direction as `_src`/`_tgt`.
fn load_graph_with_arcs(path: &Path, limit: u64) -> Result<Graph> {
    security::check_graph_file_size_cap_with(path, limit)?;
    let mut data = read_json(path)?;
    if data.get("links").is_none() && data.get("edges").is_some() {
        data["links"] = data["edges"].take();
    }
    crate::build::mark_parallel_links(&mut data);
    Graph::from_node_link_owned_with_arcs(data, "links")
}
pub fn file_hash(path: &Path) -> Result<String> {
    let digest = sha2::Sha256::digest(fs::read(path).map_err(crate::Error::from)?);
    Ok(format!("{digest:x}")[..16].to_string())
}

pub fn dispatch(request: &Value) -> Result<Value> {
    let operation = request["operation"].as_str().unwrap_or("");
    if operation == "global.prefix" {
        let graph = Graph::from_node_link(&request["graph"], "links")?;
        return Ok(build_global::prefix_graph_for_global(
            &graph,
            request["tag"].as_str().ok_or("Expected tag")?,
            request["offset"].as_i64().unwrap_or(0),
        )?
        .to_node_link("links"));
    }
    if operation == "global.prune" {
        let mut graph = Graph::from_node_link(&request["graph"], "links")?;
        let count = build_global::prune_repo_from_graph(
            &mut graph,
            request["tag"].as_str().ok_or("Expected tag")?,
        )?;
        return Ok(json!({"removed":count,"graph":graph.to_node_link("links")}));
    }
    if operation == "global.tags" {
        let paths: Vec<String> =
            serde_json::from_value(request["paths"].clone()).map_err(|_| "Expected paths")?;
        return Ok(json!(build_global::distinct_repo_tags(&paths)));
    }
    if operation == "global.hash" {
        return Ok(json!(file_hash(Path::new(
            request["path"].as_str().ok_or("Expected path")?
        ))?));
    }
    let mut store = match request["directory"].as_str() {
        Some(s) => GlobalStore::new(s.into()),
        None => GlobalStore::default_location()?,
    };
    if let Some(s) = request["graph_path"].as_str() {
        store.graph_path = s.into();
    }
    if let Some(s) = request["manifest_path"].as_str() {
        store.manifest_path = s.into();
    }
    if let Some(v) = request["limit"].as_u64() {
        store.graph_byte_limit = v;
    }
    if let Some(s) = request["now"].as_str() {
        store.now = Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(crate::Error::from)?
                .with_timezone(&Utc),
        );
    }
    let value = match operation {
        "global.load_manifest" => store.load_manifest(),
        "global.save_manifest" => {
            store.save_manifest(&request["value"])?;
            Value::Null
        }
        "global.load_graph" => store.load_graph()?.to_node_link("links"),
        "global.save_graph" => {
            store.save_graph(&Graph::from_node_link(&request["graph"], "links")?)?;
            Value::Null
        }
        "global.add" => store.add(
            Path::new(request["path"].as_str().ok_or("Expected source path")?),
            request["tag"].as_str().ok_or("Expected tag")?,
        )?,
        "global.remove" => json!(store.remove(request["tag"].as_str().ok_or("Expected tag")?)?),
        "global.list" => store.list()?,
        "global.path" => json!(fsutil::path_string(&store.graph_path)),
        _ => return Err("Unknown global-store operation".into()),
    };
    Ok(json!({"value":value,"warnings":store.warnings}))
}
