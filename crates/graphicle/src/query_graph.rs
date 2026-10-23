//! Saved code graphs: each recorded episode's graph and search index, built
//! once after its projection and read by every later process instead of
//! rebuilding them from the store's rows (GitHub issue #8: a question on a
//! large project spent most of its time there).
//!
//! The saved graph is `graphicle::snapshot`'s encoding of the graph exactly as
//! [`graph_projection::load`] reads it from the store, with its search index,
//! keyed by the episode and readable by the build that wrote it only. It
//! lives in the project's graph cache folder (`gf-cache/<project>/query/`),
//! which goes with the project. Beside it, the episode's file relationships
//! (what `query::episode_graph` lists as `edges`) are kept as pairs of files,
//! so `context` reads the relationships of the files it needs without the
//! whole list ([`FileEdges`]).
//!
//! A missing, unreadable or stale file is never an error: the graph is then
//! read from the store as before, and the next projection or sync writes it
//! again ([`ensure`]).
use crate::{Result, graph_projection};
use graphicle::graph::Graph;
use graphicle::serve_search::SearchIndex;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Changes when what this module writes changes.
const FORMAT: &str = "graphicle.query_graph.v1";

/// The folder of a project's saved graphs: `gf-cache/<project>/query`.
fn folder(db: &Connection, repository: &str) -> Option<PathBuf> {
    Some(graph_projection::cache_root(db, repository)?.join("query"))
}

fn episode_name(episode: &str) -> Option<&str> {
    (!episode.is_empty()
        && episode
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(episode)
}

fn graph_file(db: &Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    Some(folder(db, repository)?.join(format!("{}.graph", episode_name(episode)?)))
}

fn edges_file(db: &Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    Some(folder(db, repository)?.join(format!("{}.files", episode_name(episode)?)))
}

fn key(repository: &str, episode: &str) -> Vec<u8> {
    format!("{FORMAT}\0{repository}\0{episode}").into_bytes()
}

/// The saved graph and index of `episode`, when this build wrote one.
pub fn read(db: &Connection, repository: &str, episode: &str) -> Option<(Graph, SearchIndex)> {
    if !enabled() {
        return None;
    }
    let path = graph_file(db, repository, episode)?;
    let bytes = std::fs::read(path).ok()?;
    graphicle::snapshot::from_bytes(&bytes, &key(repository, episode))?.ok()
}

/// Whether saved graphs are read and written: `GRAPHICLE_SAVED_GRAPH=0`
/// turns them off (the graph is then read from the store every time).
pub fn enabled() -> bool {
    !std::env::var("GRAPHICLE_SAVED_GRAPH").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Whether `episode` has a saved graph this build can read, without reading
/// it (its header only).
pub fn saved(db: &Connection, repository: &str, episode: &str) -> bool {
    enabled()
        && graph_file(db, repository, episode)
            .and_then(|path| {
                use std::io::Read;
                let mut head = vec![0u8; 4096];
                let mut file = std::fs::File::open(path).ok()?;
                let n = file.read(&mut head).ok()?;
                head.truncate(n);
                Some(graphicle::snapshot::from_bytes(&head, &key(repository, episode)).is_some())
            })
            .unwrap_or(false)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Save the graph of the project's latest recorded episode (the one
/// `context` reads) unless it is saved already, and remove the project's
/// other saved graphs. Returns what it did, for the sync's report.
pub fn ensure(db: &Connection, repository: &str) -> Result<Value> {
    if !enabled() {
        return Ok(serde_json::json!({"status":"off"}));
    }
    let Some(latest) = crate::store::one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?
    else {
        return Ok(serde_json::json!({"status":"no_episode"}));
    };
    let episode = crate::store::s(&latest, "episode").to_string();
    let (Some(graph_path), Some(edges_path)) = (
        graph_file(db, repository, &episode),
        edges_file(db, repository, &episode),
    ) else {
        return Ok(serde_json::json!({"status":"no_folder"}));
    };
    if saved(db, repository, &episode) && edges_path.is_file() {
        return Ok(serde_json::json!({"status":"saved","episode":episode}));
    }
    let started = std::time::Instant::now();
    // The graph as every other process reads it from the store.
    let graph = graph_projection::load(db, repository, &episode)?;
    let index = SearchIndex::new(&graph);
    let bytes = graphicle::snapshot::to_bytes(&key(repository, &episode), &graph, &index);
    let edges = FileEdges::of(&graph).encode(&key(repository, &episode));
    write_atomic(&graph_path, &bytes)?;
    write_atomic(&edges_path, &edges)?;
    // Only the latest episode is read by `context`; the others go.
    if let Some(folder) = graph_path.parent()
        && let Ok(entries) = std::fs::read_dir(folder)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem != episode
                && matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("graph" | "files")
                )
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(serde_json::json!({"status":"written","episode":episode,
        "bytes":bytes.len() + edges.len(),"seconds":(started.elapsed().as_secs_f64() * 10.0).round() / 10.0}))
}

/// The file relationships of an episode, as `query::episode_graph` lists
/// them (`edges`): pairs of project files with the relation kind, and how
/// many edges that list holds before and after folding by kind.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileEdges {
    pub paths: Vec<String>,
    /// (source, target, kind) by index into `paths` and `kinds`, distinct,
    /// in first-listed order.
    pub pairs: Vec<(u32, u32, u16)>,
    /// How many of `episode_graph`'s edges each pair holds (one per line).
    pub lines: Vec<u32>,
    pub kinds: Vec<String>,
    /// `episode_graph`'s edge count (one per source, target, line and kind).
    pub listed: usize,
}

fn node_text<'a>(attributes: &'a impl graphicle::record::Properties, key: &str) -> Option<&'a str> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

impl FileEdges {
    /// `episode_graph`'s edges of `graph`, folded to pairs of files.
    pub fn of(graph: &Graph) -> Self {
        let nodes = graph.nodes();
        let relpath = |index: usize| {
            let attributes = &nodes[index].attributes;
            node_text(attributes, "relpath")
                .or_else(|| node_text(attributes, "source_file"))
                .map(|p| p.replace('\\', "/"))
        };
        let node_line = |index: usize| crate::query::node_line_of(&nodes[index].attributes);
        let mut paths: Vec<String> = Vec::new();
        let mut path_ids: HashMap<String, u32> = HashMap::new();
        let mut kinds: Vec<String> = Vec::new();
        let mut kind_ids: HashMap<String, u16> = HashMap::new();
        let mut seen = BTreeSet::new();
        let mut pairs_seen: HashMap<(u32, u32, u16), usize> = HashMap::new();
        let mut pairs = Vec::new();
        let mut lines: Vec<u32> = Vec::new();
        let mut listed = 0usize;
        for (source, target, edge) in graph.iter_edges() {
            let relation = node_text(&edge.attributes, "relation").unwrap_or("");
            if relation.is_empty()
                || graphicle::markdown_resolution::CONTAINMENT_RELATIONS.contains(&relation)
            {
                continue;
            }
            let (Some(from), Some(to)) = (relpath(source), relpath(target)) else {
                continue;
            };
            if from == to {
                continue;
            }
            let line = edge
                .attributes
                .get("source_location")
                .and_then(Value::as_str)
                .and_then(|l| l.strip_prefix('L'))
                .and_then(|l| l.split(['-', ':']).next()?.trim().parse::<u64>().ok())
                .or_else(|| node_line(source))
                .unwrap_or(1);
            let kind = match relation {
                "imports" | "imports_from" => "imports",
                other => other,
            };
            if !seen.insert((from.clone(), to.clone(), line, kind.to_string())) {
                continue;
            }
            listed += 1;
            let mut id = |path: String| -> u32 {
                if let Some(&id) = path_ids.get(&path) {
                    return id;
                }
                let id = paths.len() as u32;
                paths.push(path.clone());
                path_ids.insert(path, id);
                id
            };
            let (a, b) = (id(from), id(to));
            let k = *kind_ids.entry(kind.to_string()).or_insert_with(|| {
                kinds.push(kind.to_string());
                (kinds.len() - 1) as u16
            });
            match pairs_seen.get(&(a, b, k)) {
                Some(&at) => lines[at] += 1,
                None => {
                    pairs_seen.insert((a, b, k), pairs.len());
                    pairs.push((a, b, k));
                    lines.push(1);
                }
            }
        }
        Self {
            paths,
            pairs,
            lines,
            kinds,
            listed,
        }
    }

    fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, mut v: u64| loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        };
        put(&mut out, key.len() as u64);
        out.extend_from_slice(key);
        put(&mut out, self.listed as u64);
        for table in [&self.paths, &self.kinds] {
            put(&mut out, table.len() as u64);
            for text in table.iter() {
                put(&mut out, text.len() as u64);
                out.extend_from_slice(text.as_bytes());
            }
        }
        put(&mut out, self.pairs.len() as u64);
        for ((a, b, k), n) in self.pairs.iter().zip(&self.lines) {
            put(&mut out, u64::from(*a));
            put(&mut out, u64::from(*b));
            put(&mut out, u64::from(*k));
            put(&mut out, u64::from(*n));
        }
        out
    }

    fn decode(bytes: &[u8], key: &[u8]) -> Option<Self> {
        let mut position = 0usize;
        let get = |position: &mut usize| -> Option<u64> {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = *bytes.get(*position)?;
                *position += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Some(value);
                }
                shift += 7;
                if shift > 63 {
                    return None;
                }
            }
        };
        let length = get(&mut position)? as usize;
        if bytes.get(position..position + length)? != key {
            return None;
        }
        position += length;
        let listed = get(&mut position)? as usize;
        let mut tables: Vec<Vec<String>> = Vec::new();
        for _ in 0..2 {
            let count = get(&mut position)? as usize;
            let mut table = Vec::with_capacity(count.min(1 << 20));
            for _ in 0..count {
                let n = get(&mut position)? as usize;
                let text = std::str::from_utf8(bytes.get(position..position + n)?).ok()?;
                position += n;
                table.push(text.to_string());
            }
            tables.push(table);
        }
        let count = get(&mut position)? as usize;
        let mut pairs = Vec::with_capacity(count.min(1 << 24));
        let mut lines = Vec::with_capacity(count.min(1 << 24));
        for _ in 0..count {
            pairs.push((
                get(&mut position)? as u32,
                get(&mut position)? as u32,
                get(&mut position)? as u16,
            ));
            lines.push(get(&mut position)? as u32);
        }
        let kinds = tables.pop()?;
        let paths = tables.pop()?;
        (position == bytes.len() && lines.iter().map(|n| *n as usize).sum::<usize>() == listed)
            .then_some(Self {
                paths,
                pairs,
                lines,
                kinds,
                listed,
            })
    }

    /// The saved file relationships of `episode`, when this build wrote them.
    pub fn read(db: &Connection, repository: &str, episode: &str) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let bytes = std::fs::read(edges_file(db, repository, episode)?).ok()?;
        Self::decode(&bytes, &key(repository, episode))
    }

    /// The pairs touching any of `files`, as `episode_graph`'s edges
    /// (`source`, `target` and `kind`).
    pub fn touching(&self, files: &BTreeSet<&str>) -> Vec<Value> {
        let ids: BTreeSet<u32> = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| files.contains(path.as_str()))
            .map(|(i, _)| i as u32)
            .collect();
        self.pairs
            .iter()
            .filter(|(a, b, _)| ids.contains(a) || ids.contains(b))
            .map(|(a, b, k)| {
                serde_json::json!({"source":self.paths[*a as usize],"target":self.paths[*b as usize],
                    "kind":self.kinds[*k as usize]})
            })
            .collect()
    }

    /// The files any pair joins to one of `files`.
    pub fn neighbours(&self, files: &BTreeSet<&str>) -> BTreeSet<String> {
        let ids: BTreeMap<u32, ()> = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| files.contains(path.as_str()))
            .map(|(i, _)| (i as u32, ()))
            .collect();
        let mut found = BTreeSet::new();
        for (a, b, _) in &self.pairs {
            if ids.contains_key(a) {
                found.insert(self.paths[*b as usize].clone());
            }
            if ids.contains_key(b) {
                found.insert(self.paths[*a as usize].clone());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_edges_round_trip() {
        let edges = FileEdges {
            paths: vec!["a.rs".into(), "b.rs".into()],
            pairs: vec![(0, 1, 0), (1, 0, 1)],
            lines: vec![2, 1],
            kinds: vec!["imports".into(), "calls".into()],
            listed: 3,
        };
        let bytes = edges.encode(b"k");
        assert_eq!(FileEdges::decode(&bytes, b"k"), Some(edges.clone()));
        assert_eq!(FileEdges::decode(&bytes, b"other"), None);
        let a = BTreeSet::from(["a.rs"]);
        assert_eq!(edges.touching(&a).len(), 2);
        assert_eq!(edges.neighbours(&a), BTreeSet::from(["b.rs".to_string()]));
    }
}
