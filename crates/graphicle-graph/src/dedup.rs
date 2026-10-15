// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/dedup.py
//! Source-aware entity deduplication, preserving incremental and provenance rules.

#[derive(Clone, Copy)]
pub struct TiebreakOptions<'a> {
    pub backend: &'a str,
    pub batch_size: usize,
    pub low: f64,
    pub high: f64,
}
pub use crate::dedup_strings::{
    content_token_swap, entropy, is_variant_pair, make_minhash, norm, numeric_tokens_differ,
    same_word_variant, shingles, short_label_blocked,
};
use crate::{
    Result, dedup_strings as strings,
    extractors::resolution::field,
    fsutil, ids,
    minhash::MinHashLsh,
    python_compat,
    record::{EdgeItem, Record},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};
/// hashbrown's default (foldhash, randomly seeded) tables.
type FastMap<K, V> = hashbrown::HashMap<K, V>;
type FastSet<T> = hashbrown::HashSet<T>;

pub const ENTROPY_THRESHOLD: f64 = 2.5;
pub const LSH_THRESHOLD: f64 = 0.7;
pub const MERGE_THRESHOLD: f64 = 92.0;
pub const COMMUNITY_BOOST: f64 = 5.0;
pub const NUM_PERM: usize = 128;
fn text<'a>(node: &'a Value, key: &str) -> &'a str {
    field(node, key).and_then(Value::as_str).unwrap_or("")
}
fn id(node: &Value) -> &str {
    text(node, "id")
}
fn label(node: &Value) -> &Value {
    field(node, "label").unwrap_or_else(|| field(node, "id").unwrap_or(&Value::Null))
}
fn source(node: &Value) -> &str {
    text(node, "source_file")
}
fn anchored(node: &Value) -> bool {
    matches!(text(node, "file_type"), "rationale" | "document")
}
pub fn is_code(node: &Value) -> bool {
    text(node, "file_type") == "code"
}
pub fn crossfile_fileanchored_blocked(node: &Value, neighbor: &Value) -> bool {
    (anchored(node) || anchored(neighbor)) && source(node) != source(neighbor)
}
pub fn id_prefixes(source_file: &str) -> BTreeSet<String> {
    let mut stem = source_file.replace('\\', "/");
    if let Some(i) = stem.rfind('.')
        && i + 1 < stem.len()
        && !stem[i + 1..].contains('/')
    {
        stem.truncate(i);
    }
    let segments: Vec<_> = stem
        .split('/')
        .map(ids::normalize_id)
        .filter(|s| !s.is_empty())
        .collect();
    (0..segments.len())
        .map(|i| segments[i..].join("_"))
        .collect()
}
pub fn defines_id(node: &Value) -> bool {
    !id(node).is_empty()
        && !source(node).is_empty()
        && id_prefixes(source(node))
            .iter()
            .any(|p| id(node) == p || id(node).starts_with(&format!("{p}_")))
}
pub fn reads_as_file_entity(node: &Value) -> bool {
    !id(node).is_empty()
        && !source(node).is_empty()
        && !matches!(text(node, "node_kind"), "page" | "heading")
        && !id_prefixes(source(node)).contains(id(node))
}
pub fn lifecycle_penalty(path: &str) -> u8 {
    const ACTIVE: &[&str] = &["in-progress", "in_progress", "active", "current", "wip"];
    const ARCHIVED: &[&str] = &[
        "_done",
        "done",
        "archive",
        "archived",
        "backup",
        "bak",
        "old",
        "attic",
        "graveyard",
        "completed",
    ];
    let folded = strings::casefold(path);
    let segments: Vec<_> = folded.split('/').filter(|s| !s.is_empty()).collect();
    segments[..segments.len().saturating_sub(1)]
        .iter()
        .filter_map(|s| {
            if ACTIVE.contains(s) {
                Some(0)
            } else if ARCHIVED.contains(s) {
                Some(2)
            } else {
                None
            }
        })
        .min()
        .unwrap_or(1)
}
/// `_rank_path`: the root-relative form of `source_file` used for collision
/// ranking. Only an absolute stored path is resolved and relativized; a
/// relative one is already root-relative and is never resolved against the
/// working directory (0.9.86, #4281).
pub fn rank_path(source_file: &str, root: Option<&Path>) -> String {
    let normalized = source_file.replace('\\', "/");
    if !normalized.is_empty()
        && Path::new(&normalized).is_absolute()
        && let Some(root) = root
        && let Ok(resolved) = fsutil::resolve_missing(Path::new(&normalized))
        && let Ok(relative) = resolved.strip_prefix(root)
    {
        return fsutil::path_string(relative);
    }
    normalized
}
pub type CollisionRank = (bool, u8, usize, String, Vec<String>);
/// `(id, root-relative file)` pairs read as a mention of a document ([`mentions`]).
pub type Mentions = HashSet<(String, String)>;
pub fn collision_rank(node: &Value, root: Option<&Path>) -> CollisionRank {
    collision_rank_with(node, root, &Mentions::new())
}
/// `_collision_rank(node, root, mentions)` (0.9.88, #4281): a node read as a
/// mention of a document ranks as a reference even though its id is a path
/// slice, so a real document outranks it whatever the label lengths are.
pub fn collision_rank_with(
    node: &Value,
    root: Option<&Path>,
    mentions: &Mentions,
) -> CollisionRank {
    let label = text(node, "label");
    let path = rank_path(source(node), root);
    let mentioned = !mentions.is_empty() && mentions.contains(&(id(node).to_owned(), path.clone()));
    (
        !defines_id(node) || mentioned,
        lifecycle_penalty(&path),
        label.chars().count(),
        label.into(),
        path.split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .rev()
            .map(str::to_owned)
            .collect(),
    )
}
/// `_EXTENSION.search(path)` (`\.[^./]+$`): the final `.ext` of a path.
fn extension_match(path: &str) -> Option<&str> {
    let i = path.rfind('.')?;
    let tail = &path[i + 1..];
    (!tail.is_empty() && !tail.contains('/')).then(|| &path[i..])
}
/// `_full_stem` (0.9.85): the extension-stripped, slugified full-path stem a
/// file's own IDs start with.
pub fn full_stem(source_file: &str) -> String {
    let normalized = source_file.replace('\\', "/");
    let stem = match extension_match(&normalized) {
        Some(ext) => &normalized[..normalized.len() - ext.len()],
        None => normalized.as_str(),
    };
    stem.split('/')
        .map(ids::normalize_id)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}
/// `_extension_qualified_id` (0.9.85, #4281): `x.html`'s own spelling of an ID
/// it shares with `x.py`. Only an ID spelled with the node's FULL path stem
/// qualifies; a bare trailing slice is a reference, not a distinct entity.
pub fn extension_qualified_id(node: &Value) -> Option<String> {
    let nid = id(node);
    let source_file = source(node);
    let normalized = source_file.replace('\\', "/");
    let ext = extension_match(&normalized)?;
    let full = full_stem(source_file);
    if nid.is_empty() || full.is_empty() {
        return None;
    }
    if nid != full && !nid.starts_with(&format!("{full}_")) {
        return None;
    }
    Some(format!(
        "{full}_{}{}",
        ids::normalize_id(ext),
        &nid[full.len()..]
    ))
}
fn collision_key(source_file: Option<&Value>, root: Option<&Path>) -> String {
    source_file
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| rank_path(s, root))
        .unwrap_or_default()
}
/// Apply `(old id, file key) -> new id` renames to edge endpoints and
/// hyperedge members, matched by each item's own `source_file`.
fn rename_references<R: Record, E: EdgeItem<Record = R>>(
    items: &mut [E],
    renames: &HashMap<(String, String), String>,
    root: Option<&Path>,
) {
    for item in items.iter_mut() {
        let Some(item) = item.record_mut() else {
            continue;
        };
        let k = collision_key(item.get("source_file"), root);
        for end in ["source", "target"] {
            if let Some(new) = item
                .get(end)
                .and_then(Value::as_str)
                .and_then(|old| renames.get(&(old.to_owned(), k.clone())))
            {
                item.put(end, json!(new));
            }
        }
        // Members are renamed in a copy that replaces the list in place.
        let renamed = match item.get("nodes") {
            Some(Value::Array(members)) => {
                let mut renamed: Option<Vec<Value>> = None;
                for (i, member) in members.iter().enumerate() {
                    if let Some(new) = member
                        .as_str()
                        .and_then(|old| renames.get(&(old.to_owned(), k.clone())))
                    {
                        renamed.get_or_insert_with(|| members.clone())[i] = json!(new);
                    }
                }
                renamed
            }
            _ => None,
        };
        if let Some(members) = renamed {
            item.put("nodes", Value::Array(members));
        }
    }
}
/// `_disambiguate_defining_collisions` (0.9.85, #4281): keep `x.py` and
/// `x.html` apart when both derive the same ID from the extension-stripped
/// path. The best-ranked file keeps it; the others get an extension-qualified
/// ID, and edges and hyperedges follow the renamed node by their own
/// root-relative `source_file`. Anything else stays on collapse-and-warn.
pub fn disambiguate_defining_collisions(
    nodes: &mut [Value],
    edges: &mut [Value],
    hyperedges: Option<&mut [Value]>,
    root: Option<&Path>,
) {
    disambiguate_defining_collisions_in(nodes, edges, hyperedges, root);
}
/// [`disambiguate_defining_collisions`] over edges of either form.
fn disambiguate_defining_collisions_in<R: Record, E: EdgeItem<Record = R>>(
    nodes: &mut [Value],
    edges: &mut [E],
    hyperedges: Option<&mut [Value]>,
    root: Option<&Path>,
) {
    let key = |node: &Value| collision_key(field(node, "source_file"), root);
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        let nid = id(node);
        if !nid.is_empty() && !source(node).is_empty() && extension_qualified_id(node).is_some() {
            by_id
                .entry(nid.to_owned())
                .or_insert_with(|| {
                    order.push(nid.to_owned());
                    Vec::new()
                })
                .push(i);
        }
    }
    let mut taken: HashSet<String> = nodes
        .iter()
        .map(id)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let mut renames: HashMap<(String, String), String> = HashMap::new();
    for nid in &order {
        let definers = &by_id[nid];
        if definers
            .iter()
            .map(|&i| key(&nodes[i]))
            .collect::<HashSet<_>>()
            .len()
            < 2
        {
            continue;
        }
        let winner = *definers
            .iter()
            .min_by(|&&a, &&b| {
                collision_rank(&nodes[a], root).cmp(&collision_rank(&nodes[b], root))
            })
            .expect("at least two definers");
        let winner_key = key(&nodes[winner]);
        let winner_source = source(&nodes[winner]);
        let mut sorted = definers.clone();
        sorted.sort_by_cached_key(|&i| key(&nodes[i]));
        for i in sorted {
            let k = key(&nodes[i]);
            if k == winner_key || renames.contains_key(&(nid.clone(), k.clone())) {
                continue;
            }
            let d_source = source(&nodes[i]);
            // Same full stem only: x.py vs x.html, never a/x vs b/x.
            if full_stem(d_source) != full_stem(winner_source) {
                continue;
            }
            // Same-extension slug twins stay on the collapse-and-warn path.
            if let (Some(a), Some(b)) = (extension_match(d_source), extension_match(winner_source))
                && a.to_lowercase() == b.to_lowercase()
            {
                continue;
            }
            if let Some(new_id) = extension_qualified_id(&nodes[i])
                && taken.insert(new_id.clone())
            {
                renames.insert((nid.clone(), k), new_id);
            }
        }
    }
    if renames.is_empty() {
        return;
    }
    for node in nodes.iter_mut() {
        if let Some(new) = renames.get(&(id(node).to_owned(), key(node))) {
            node["id"] = json!(new);
        }
    }
    rename_references(edges, &renames, root);
    if let Some(hyperedges) = hyperedges {
        rename_references(hyperedges, &renames, root);
    }
}
/// `_mentions` (0.9.88, #4281): `(id, root-relative file)` of nodes read as a
/// MENTION of a document: another node of the same file points at it with a
/// `references` edge of that file, and its id is a path slice of that file.
/// Inferred from the edges an extractor chose to emit, not stated by the source.
pub fn mentions(nodes: &[Value], edges: &[Value], root: Option<&Path>) -> Mentions {
    mentions_in(nodes, edges, root)
}
/// [`mentions`] over edges of either form.
fn mentions_in<R: Record, E: EdgeItem<Record = R>>(
    nodes: &[Value],
    edges: &[E],
    root: Option<&Path>,
) -> Mentions {
    let mut own: HashMap<String, HashSet<&str>> = HashMap::new();
    for node in nodes {
        let nid = id(node);
        if !nid.is_empty() {
            own.entry(collision_key(field(node, "source_file"), root))
                .or_default()
                .insert(nid);
        }
    }
    let mut found = Mentions::new();
    for edge in edges {
        let Some(edge) = edge.record() else {
            continue;
        };
        if edge.get("relation").and_then(Value::as_str) != Some("references") {
            continue;
        }
        let k = collision_key(edge.get("source_file"), root);
        let (Some(source), Some(target)) = (
            edge.get("source").and_then(Value::as_str),
            edge.get("target").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !k.is_empty()
            && source != target
            && own
                .get(&k)
                .is_some_and(|own| own.contains(source) && own.contains(target))
            && id_prefixes(&k).contains(target)
        {
            found.insert((target.to_owned(), k));
        }
    }
    found
}
/// The non-empty string `id` and `source_file` of a node, else None.
fn id_and_source(node: &Value) -> Option<(&str, &str)> {
    let nid = field(node, "id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let sf = field(node, "source_file")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some((nid, sf))
}
/// `(rank path, full stem)` of a stored `source_file`, memoized per call
/// (many nodes share a file).
fn ranked_file<'a>(
    memo: &mut HashMap<&'a str, (String, String)>,
    sf: &'a str,
    root: Option<&Path>,
) -> (String, String) {
    memo.entry(sf)
        .or_insert_with(|| {
            let k = rank_path(sf, root);
            let full = full_stem(&k);
            (k, full)
        })
        .clone()
}
/// `_own_page_files` (0.9.91, #4281): root-relative files that have a node
/// spelled with their own full stem. Such a file already has its page, so a
/// shorter path-slice id it also mints (`protocol` inside docs/protocol.md,
/// next to `docs_protocol`) may be a reference to another file.
pub fn own_page_files(nodes: &[Value], root: Option<&Path>) -> HashSet<String> {
    let mut memo = HashMap::new();
    let mut files = HashSet::new();
    for node in nodes {
        let Some((nid, sf)) = id_and_source(node) else {
            continue;
        };
        let (k, full) = ranked_file(&mut memo, sf, root);
        if crate::paths::is_absolute_any_platform(Some(&k)) {
            continue;
        }
        if nid == full
            || nid
                .strip_prefix(full.as_str())
                .is_some_and(|rest| rest.starts_with('_'))
        {
            files.insert(k);
        }
    }
    files
}
/// `_slice_references` (0.9.91, #4281): `(id, file)` of nodes whose id is a
/// SHORTER path slice of a file that has its own page. They rank as
/// references, like a [`mentions`] entry.
pub fn slice_references(nodes: &[Value], root: Option<&Path>) -> Mentions {
    let own_page = own_page_files(nodes, root);
    let mut found = Mentions::new();
    if own_page.is_empty() {
        return found;
    }
    let mut memo = HashMap::new();
    for node in nodes {
        let Some((nid, sf)) = id_and_source(node) else {
            continue;
        };
        let (k, full) = ranked_file(&mut memo, sf, root);
        if own_page.contains(&k) && nid != full && id_prefixes(&k).contains(nid) {
            found.insert((nid.to_owned(), k));
        }
    }
    found
}
/// `str(node.get("id") or "")`.
fn id_text(node: &Value) -> String {
    match field(node, "id") {
        Some(value) if crate::build_normalize::truthy(value) => python_compat::string(value),
        _ => String::new(),
    }
}
/// `_path_qualify_dropped_collisions` (0.9.88-0.9.91, #4281/#4362): a
/// semantic id minted from a SHORTER-than-full path stem by files of different
/// full stems (`a/x.md` and `b/x.md` both emitting `x`) is renamed per file to
/// `<full path stem><rest of id>`; no file keeps the bare id. AST nodes, an
/// id already carrying the full stem, a path that stays absolute and a mention
/// (see [`mentions`]) are left alone. The file whose full stem IS the id
/// (`protocol.md` for `protocol`) defines it, keeps the bare id and counts
/// toward the collision (0.9.91), so `docs/protocol.md` next to it is still
/// renamed; a candidate whose bare id names another input file is renamed
/// only if its own file has no other node spelled with its own full stem
/// ([`own_page_files`]). A reference to the bare id from another file cannot
/// be attributed and is left unresolved, with one warning per id.
pub fn path_qualify_dropped_collisions(
    nodes: &mut [Value],
    edges: &mut [Value],
    hyperedges: Option<&mut [Value]>,
    root: Option<&Path>,
    stderr: &mut Vec<String>,
) {
    path_qualify_dropped_collisions_in(nodes, edges, hyperedges, root, stderr);
}
/// [`path_qualify_dropped_collisions`] over edges of either form.
fn path_qualify_dropped_collisions_in<R: Record, E: EdgeItem<Record = R>>(
    nodes: &mut [Value],
    edges: &mut [E],
    hyperedges: Option<&mut [Value]>,
    root: Option<&Path>,
    stderr: &mut Vec<String>,
) {
    let key = |node: &Value| collision_key(field(node, "source_file"), root);
    let qualified_id = |node: &Value| -> Option<(String, String)> {
        let nid = field(node, "id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        let source_file = field(node, "source_file")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        if crate::build_normalize::is_ast_tier(node) {
            return None;
        }
        let rel = rank_path(source_file, root);
        if crate::paths::is_absolute_any_platform(Some(&rel)) {
            // Never bake an on-disk path into an id.
            return None;
        }
        let full = full_stem(&rel);
        let longest = id_prefixes(&rel)
            .into_iter()
            .filter(|p| nid == p || nid.starts_with(&format!("{p}_")))
            .max_by_key(String::len)?;
        if full.is_empty() || longest == full {
            return None;
        }
        Some((format!("{full}{}", &nid[longest.len()..]), longest))
    };
    let named_files: HashSet<String> = nodes
        .iter()
        .filter_map(|n| field(n, "source_file").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(|s| full_stem(&rank_path(s, root)))
        .collect();
    let mentioned = mentions_in(nodes, edges, root);
    let mut by_id: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    let mut owners: HashMap<String, HashSet<String>> = HashMap::new();
    let mut kept_bare: HashSet<(String, String)> = HashSet::new();
    for (i, node) in nodes.iter().enumerate() {
        let Some(nid) = field(node, "id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let k = key(node);
        owners.entry(nid.to_owned()).or_default().insert(k.clone());
        if qualified_id(node).is_none() {
            continue;
        }
        if mentioned.contains(&(nid.to_owned(), k.clone())) {
            kept_bare.insert((nid.to_owned(), k));
        } else {
            by_id.entry(nid.to_owned()).or_default().push(i);
        }
    }
    // Files with a node spelled with their own full stem: their page already
    // exists, so a shorter-prefix id from such a file may be a reference.
    let own_page = own_page_files(nodes, root);
    let mut renames: HashMap<(String, String), String> = HashMap::new();
    let mut claimed: HashMap<String, String> = HashMap::new();
    for (nid, members) in &by_id {
        // The file whose full stem IS the id defines it: it keeps the bare id
        // and counts toward the collision although it is not a candidate.
        let definers = owners.get(nid).into_iter().flatten().filter(|o| {
            !o.is_empty()
                && !crate::paths::is_absolute_any_platform(Some(o.as_str()))
                && full_stem(o) == *nid
        });
        let files: HashSet<String> = members
            .iter()
            .map(|&i| key(&nodes[i]))
            .chain(definers.cloned())
            .collect();
        if files.len() < 2 {
            continue;
        }
        if files
            .iter()
            .map(|f| full_stem(f))
            .collect::<HashSet<_>>()
            .len()
            < 2
        {
            continue;
        }
        let mut sorted = members.clone();
        sorted.sort_by_cached_key(|&i| key(&nodes[i]));
        for i in sorted {
            let k = key(&nodes[i]);
            let pair = (nid.clone(), k.clone());
            if renames.contains_key(&pair) || kept_bare.contains(&pair) {
                continue;
            }
            // The bare id names another input file (`protocol` while
            // protocol.md exists): it is this file's own page only if the file
            // has no other page node; otherwise it may be a reference, so it
            // is left to the collapse.
            if named_files.contains(
                &qualified_id(&nodes[i])
                    .map(|(_, prefix)| prefix)
                    .unwrap_or_default(),
            ) && own_page.contains(&k)
            {
                kept_bare.insert(pair);
                continue;
            }
            let new_id = qualified_id(&nodes[i]).map(|(new_id, _)| new_id);
            let Some(new_id) = new_id.filter(|new_id| {
                !owners
                    .get(new_id)
                    .is_some_and(|o| o.iter().any(|owner| *owner != k))
                    && claimed.get(new_id).is_none_or(|owner| *owner == k)
            }) else {
                kept_bare.insert(pair);
                continue;
            };
            claimed.insert(new_id.clone(), k);
            renames.insert(pair, new_id);
        }
    }
    if renames.is_empty() {
        return;
    }
    for node in nodes.iter_mut() {
        if let Some(new) = renames.get(&(id_text(node), key(node))) {
            node["id"] = json!(new);
        }
    }
    let renamed_ids: HashSet<&str> = renames.keys().map(|(nid, _)| nid.as_str()).collect();
    let mut unattributed: std::collections::BTreeMap<String, usize> = Default::default();
    let unattributed_ids = Unattributed {
        root,
        renamed_ids: &renamed_ids,
        renames: &renames,
        kept_bare: &kept_bare,
    };
    unattributed_ids.count(edges, &mut unattributed);
    if let Some(hyperedges) = &hyperedges {
        unattributed_ids.count(hyperedges, &mut unattributed);
    }
    rename_references(edges, &renames, root);
    if let Some(hyperedges) = hyperedges {
        rename_references(hyperedges, &renames, root);
    }
    for (nid, n) in unattributed {
        stderr.push(format!("[graphicle] WARNING: node '{nid}' was minted by several files and renamed per file; {n} reference(s) from other files could not be attributed and were left unresolved."));
    }
}
/// References to a renamed id that [`path_qualify_dropped_collisions`]
/// cannot attribute to one file, counted per id.
struct Unattributed<'a> {
    root: Option<&'a Path>,
    renamed_ids: &'a HashSet<&'a str>,
    renames: &'a HashMap<(String, String), String>,
    kept_bare: &'a HashSet<(String, String)>,
}
impl Unattributed<'_> {
    fn count<R: Record, E: EdgeItem<Record = R>>(
        &self,
        items: &[E],
        unattributed: &mut std::collections::BTreeMap<String, usize>,
    ) {
        for item in items {
            let Some(item) = item.record() else {
                continue;
            };
            let k = collision_key(item.get("source_file"), self.root);
            let members = item.get("nodes").and_then(Value::as_array);
            let refs = [item.get("source"), item.get("target")]
                .into_iter()
                .flatten()
                .chain(members.into_iter().flatten());
            for reference in refs.filter_map(Value::as_str) {
                let pair = (reference.to_owned(), k.clone());
                if self.renamed_ids.contains(reference)
                    && !self.renames.contains_key(&pair)
                    && !self.kept_bare.contains(&pair)
                {
                    *unattributed.entry(reference.to_owned()).or_default() += 1;
                }
            }
        }
    }
}
pub fn same_source_entity(survivor: &Value, duplicate: &Value) -> bool {
    !source(survivor).is_empty() && source(survivor) == source(duplicate)
}
pub fn merge_missing_attributes(survivor: &Value, duplicate: &Value) -> Value {
    let mut merged = survivor.clone();
    if let (Some(merged), Some(duplicate)) = (merged.as_object_mut(), duplicate.as_object()) {
        for (key, value) in duplicate {
            if key != "_origin" && !value.is_null() && merged.get(key).is_none_or(Value::is_null) {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    merged
}
pub fn report_id_collision(nid: &str, survivor: &Value, losers: &[Value]) -> Vec<String> {
    let mut messages = Vec::new();
    let keep_file = source(survivor);
    let keep_label = text(survivor, "label");
    for loser in losers {
        let lose_file = source(loser);
        let lose_label = text(loser, "label");
        if lose_file == keep_file {
            if norm(&json!(lose_label)) != norm(&json!(keep_label)) {
                messages.push(format!("[graphicle] note: node '{nid}' was extracted twice from '{keep_file}' under different labels — keeping '{keep_label}', dropping '{lose_label}'."));
            }
        } else if !defines_id(survivor) || defines_id(loser) {
            messages.push(format!("[graphicle] WARNING: node '{nid}' is minted by two different files — keeping '{keep_label}' from '{keep_file}', dropping '{lose_label}' from '{lose_file}'. An ID is derived from the source path plus the entity name, so this one does not identify a single entity and the dropped node is lost. To keep them distinct, run '{program} extract' per subfolder and merge with '{program} merge-graphs'.", program = crate::cli::program()));
        }
    }
    messages
}
pub fn remap_hyperedge_members(hyperedges: &mut [Value], remap: &HashMap<String, String>) {
    for he in hyperedges {
        let Some(members) = he.get_mut("nodes").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut seen = HashSet::new();
        let mut rewired = Vec::new();
        for mut member in std::mem::take(members) {
            let member_id = if let Some(s) = member.as_str() {
                Some(s.to_owned())
            } else {
                member.get("id").and_then(Value::as_str).map(str::to_owned)
            };
            if let Some(raw) = member_id {
                let new_id = remap.get(&raw).unwrap_or(&raw);
                if !seen.insert(new_id.clone()) {
                    continue;
                }
                if member.is_string() {
                    member = json!(new_id);
                } else if new_id != &raw {
                    member["id"] = json!(new_id);
                }
            }
            rewired.push(member);
        }
        *members = rewired;
    }
}
#[derive(Default, Clone, Debug)]
pub struct UnionFind {
    parent: HashMap<String, String>,
    order: Vec<String>,
}
impl UnionFind {
    fn insert(&mut self, x: &str) {
        if !self.parent.contains_key(x) {
            self.parent.insert(x.into(), x.into());
            self.order.push(x.into());
        }
    }
    pub fn find(&mut self, x: &str) -> String {
        self.insert(x);
        let mut x = x.to_owned();
        while self.parent[&x] != x {
            let p = self.parent[&self.parent[&x]].clone();
            self.parent.insert(x.clone(), p.clone());
            x = p;
        }
        x
    }
    pub fn union(&mut self, x: &str, y: &str) {
        self.insert(x);
        self.insert(y);
        let a = self.find(x);
        let b = self.find(y);
        if a != b {
            self.parent.insert(b, a);
        }
    }
    pub fn components(&mut self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        let mut indices = HashMap::new();
        for x in self.order.clone() {
            let root = self.find(&x);
            let next = out.len();
            let i = *indices.entry(root.clone()).or_insert_with(|| {
                out.push((root, Vec::new()));
                next
            });
            out[i].1.push(x);
        }
        out
    }
}
struct ProtectedUnion {
    pub uf: UnionFind,
    protected: HashSet<String>,
    by_root: HashMap<String, String>,
}
impl ProtectedUnion {
    fn new(protected: &HashSet<String>, uf: UnionFind) -> Self {
        Self {
            uf,
            protected: protected.clone(),
            by_root: protected.iter().map(|s| (s.clone(), s.clone())).collect(),
        }
    }
    fn get(&mut self, id: &str) -> Option<String> {
        let root = self.uf.find(id);
        self.by_root.get(&root).cloned()
    }
    fn blocked(&mut self, a: &str, b: &str) -> bool {
        if self.protected.is_empty() {
            return false;
        }
        matches!((self.get(a),self.get(b)),(Some(x),Some(y)) if x != y)
    }
    fn union(&mut self, a: &str, b: &str) {
        let x = self.get(a);
        let y = self.get(b);
        self.uf.union(a, b);
        let root = self.uf.find(a);
        if let Some(p) = x.or(y) {
            self.by_root.insert(root, p);
        }
    }
}
pub fn content_richness(node: &Value) -> usize {
    const IGNORE: &[&str] = &[
        "id",
        "label",
        "norm_label",
        "file_type",
        "source_file",
        "source_location",
    ];
    let mut score = 0;
    for (key, value) in node.as_object().into_iter().flatten() {
        if IGNORE.contains(&key.as_str())
            || value.is_null()
            || value.as_str() == Some("")
            || value.as_array().is_some_and(Vec::is_empty)
            || value.as_object().is_some_and(Map::is_empty)
        {
            continue;
        }
        score += 1;
        if key == "attributes" {
            score += value.as_object().map_or(0, Map::len);
        } else if key == "_merged_from" {
            score += value.as_array().map_or(0, |v| v.len());
        }
    }
    score
}
fn chunk_suffix(nid: &str) -> bool {
    let nid = nid.strip_suffix('\n').unwrap_or(nid);
    nid.rsplit_once("_c")
        .is_some_and(|(_, tail)| !tail.is_empty() && tail.chars().all(strings::decimal))
}
/// A node whose `source_file` is populated names a real located declaration.
/// It outranks a sourceless stub regardless of richness (upstream #3775).
fn located(node: &Value) -> bool {
    field(node, "source_file").is_some_and(crate::build_normalize::truthy)
}
/// Among located nodes, one that also carries a `source_location` outranks
/// one that does not (#3786). A location without a file is not provenance.
fn pinpointed(node: &Value) -> bool {
    located(node) && field(node, "source_location").is_some_and(crate::build_normalize::truthy)
}
fn winner_key(node: &Value) -> (bool, bool, bool, std::cmp::Reverse<usize>, usize) {
    (
        chunk_suffix(id(node)),
        !located(node),
        !pinpointed(node),
        std::cmp::Reverse(content_richness(node)),
        id(node).chars().count(),
    )
}
pub fn pick_winner(nodes: &[Value]) -> Result<&Value> {
    nodes
        .iter()
        .min_by_key(|n| winner_key(n))
        .ok_or_else(|| "Cannot pick winner from empty list".into())
}
fn pick_index(group: &[usize], nodes: &[Value]) -> usize {
    *group
        .iter()
        .min_by_key(|&&i| winner_key(&nodes[i]))
        .expect("Nonempty merge group")
}
fn score_pair(
    a: &Value,
    b: &Value,
    x: &str,
    y: &str,
    communities: &Map<String, Value>,
) -> Option<f64> {
    let nx = x.chars().count();
    let ny = y.chars().count();
    let score = if source(a) != source(b) && nx.max(ny) >= 12 {
        strings::jaro(x, y)
    } else {
        strings::jaro_winkler(x, y)
    } * 100.0;
    if is_variant_pair(x, y) || short_label_blocked(x, y, score) {
        return None;
    }
    let (lo, hi) = if nx <= ny { (x, y) } else { (y, x) };
    if hi != lo && hi.starts_with(lo) {
        return None;
    }
    if numeric_tokens_differ(x, y)
        || content_token_swap(x, y)
        || crossfile_fileanchored_blocked(a, b)
    {
        return None;
    }
    let (c1, c2) = (communities.get(id(a)), communities.get(id(b)));
    let boost = c1.is_some_and(|v| !v.is_null())
        && c2.is_some_and(|v| !v.is_null())
        && c1 == c2
        && nx.min(ny) >= 12;
    Some(score + if boost { COMMUNITY_BOOST } else { 0.0 })
}

/// Provider transport is supplied by the caller; deduplication owns batching,
/// conservative candidate guards, response interpretation and protected unions.
pub trait TiebreakClient {
    fn availability(&self, backend: &str) -> BackendAvailability;
    fn call(&mut self, prompt: &str, backend: &str, max_tokens: u32) -> crate::Result<String>;
}
pub enum BackendAvailability {
    Available,
    Unknown,
    MissingCredentials(String),
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DedupOptions {
    #[serde(default)]
    pub communities: Map<String, Value>,
    pub dedup_llm_backend: Option<String>,
    pub root: Option<PathBuf>,
    #[serde(default)]
    pub protected_ids: HashSet<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DedupOutput {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}
fn merge_exact_group(group: &[usize], nodes: &[Value], uf: &mut ProtectedUnion) -> usize {
    if group.len() <= 1 {
        return 0;
    }
    let protected: Vec<_> = group
        .iter()
        .copied()
        .filter(|&i| uf.protected.contains(id(&nodes[i])))
        .collect();
    let incoming: Vec<_> = group
        .iter()
        .copied()
        .filter(|&i| !uf.protected.contains(id(&nodes[i])))
        .collect();
    if !protected.is_empty() && incoming.is_empty() {
        return 0;
    }
    let winner = pick_index(
        if protected.is_empty() {
            group
        } else {
            &protected
        },
        nodes,
    );
    let mut count = 0;
    for &i in if protected.is_empty() {
        group
    } else {
        &incoming
    } {
        let (a, b) = (id(&nodes[winner]), id(&nodes[i]));
        if uf.blocked(a, b) {
            continue;
        }
        if uf.uf.find(a) != uf.uf.find(b) {
            uf.union(a, b);
            count += 1;
        }
    }
    count
}
/// Pass 1's cross-file join (0.9.76, #3094). Every pair is checked against
/// [`crossfile_fileanchored_blocked`], as Pass 2 already was, so document and
/// rationale nodes stay one per file while the concept nodes beside them still
/// join each other. Protected nodes stay apart; incoming nodes fold into at
/// most one protected survivor (#3477).
fn merge_crossfile_group(group: &[usize], nodes: &[Value], uf: &mut ProtectedUnion) -> usize {
    if group.len() <= 1 {
        return 0;
    }
    let (protected, incoming): (Vec<usize>, Vec<usize>) = group
        .iter()
        .copied()
        .partition(|&i| uf.protected.contains(id(&nodes[i])));
    if incoming.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut join = |a: usize, b: usize, uf: &mut ProtectedUnion| {
        let (x, y) = (id(&nodes[a]), id(&nodes[b]));
        if crossfile_fileanchored_blocked(&nodes[a], &nodes[b])
            || uf.blocked(x, y)
            || uf.uf.find(x) == uf.uf.find(y)
        {
            return;
        }
        uf.union(x, y);
        count += 1;
    };
    if !protected.is_empty() {
        let winner = pick_index(&protected, nodes);
        for &i in &incoming {
            join(winner, i, uf);
        }
    }
    if uf.protected.is_empty() && !incoming.iter().any(|&i| anchored(&nodes[i])) {
        // Nothing can block a pair: the pairwise join performs exactly these
        // unions, in this order, and then finds every later pair joined.
        for &i in &incoming[1..] {
            join(incoming[0], i, uf);
        }
        return count;
    }
    for (k, &a) in incoming.iter().enumerate() {
        for &b in &incoming[k + 1..] {
            join(a, b, uf);
        }
    }
    count
}
pub fn deduplicate_entities(
    nodes: &[Value],
    edges: &[Value],
    options: &DedupOptions,
    hyperedges: Option<&mut [Value]>,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<DedupOutput> {
    deduplicate_entities_owned(nodes.to_vec(), edges.to_vec(), options, hyperedges, client)
}
/// [`deduplicate_entities`] over owned inputs: surviving nodes and rewritten
/// edges are moved into the output instead of being copied.
pub fn deduplicate_entities_owned(
    nodes: Vec<Value>,
    edges: Vec<Value>,
    options: &DedupOptions,
    hyperedges: Option<&mut [Value]>,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<DedupOutput> {
    let (mut output, edges) = deduplicate_records(nodes, edges, options, hyperedges, client)?;
    output.edges = edges;
    Ok(output)
}
/// [`deduplicate_entities_owned`] over edges of either form: the build's
/// compact edge records are read and rewritten in place, never copied as
/// JSON. The output's `edges` stay empty; the edges come back beside it.
pub(crate) fn deduplicate_records<R: Record, E: EdgeItem<Record = R>>(
    nodes: Vec<Value>,
    edges: Vec<E>,
    options: &DedupOptions,
    mut hyperedges: Option<&mut [Value]>,
    mut client: Option<&mut dyn TiebreakClient>,
) -> Result<(DedupOutput, Vec<E>)> {
    let repos: BTreeSet<_> = nodes
        .iter()
        .filter_map(|n| n["repo"].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if repos.len() > 1 {
        return Err(format!(
            "deduplicate_entities: nodes span multiple repos {}. Cross-project dedup is disabled — run dedup per-repo before merging.",
            python_compat::repr(&json!(repos))
        ).into());
    }
    let mut output = DedupOutput {
        nodes: Vec::new(),
        edges: Vec::new(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    if nodes.len() <= 1 {
        output.nodes = nodes;
        return Ok((output, edges));
    }
    let root = options
        .root
        .as_ref()
        .filter(|p| !p.as_os_str().is_empty())
        .and_then(|p| fsutil::resolve_missing(p).ok());
    // Same-ID collisions between files that differ only by extension (0.9.85)
    // or that dropped the directory from a semantic id (0.9.88) are told apart
    // before the one-node-per-ID pass; the mention reading ranks survivors.
    let mut nodes = nodes;
    let mut edges = edges;
    disambiguate_defining_collisions_in(
        &mut nodes,
        &mut edges,
        hyperedges.as_deref_mut(),
        root.as_deref(),
    );
    path_qualify_dropped_collisions_in(
        &mut nodes,
        &mut edges,
        hyperedges.as_deref_mut(),
        root.as_deref(),
        &mut output.stderr,
    );
    let mut mentioned = mentions_in(&nodes, &edges, root.as_deref());
    mentioned.extend(slice_references(&nodes, root.as_deref()));
    // The tables below are looked up, not iterated in any order that reaches
    // the output, so they use hashbrown's faster (still randomly seeded) hasher.
    let mut unique = Vec::<Value>::with_capacity(nodes.len());
    let mut indices = FastMap::<String, usize>::with_capacity(nodes.len());
    let mut dropped = Vec::<(String, Vec<Value>)>::new();
    let mut drop_indices = FastMap::new();
    for node in nodes {
        if id(&node).is_empty() {
            continue;
        }
        if let Some(&i) = indices.get(id(&node)) {
            let nid = id(&node).to_owned();
            let loser = if collision_rank_with(&node, root.as_deref(), &mentioned)
                < collision_rank_with(&unique[i], root.as_deref(), &mentioned)
            {
                std::mem::replace(&mut unique[i], node)
            } else {
                node
            };
            let next = dropped.len();
            let j = *drop_indices.entry(nid.clone()).or_insert_with(|| {
                dropped.push((nid, Vec::new()));
                next
            });
            dropped[j].1.push(loser);
        } else {
            indices.insert(id(&node).into(), unique.len());
            unique.push(node);
        }
    }
    for (nid, losers) in &dropped {
        let i = indices[nid];
        let mut same: Vec<_> = losers
            .iter()
            .filter(|n| same_source_entity(&unique[i], n))
            .collect();
        same.sort_by_key(|n| collision_rank_with(n, root.as_deref(), &mentioned));
        for loser in same {
            unique[i] = merge_missing_attributes(&unique[i], loser);
        }
    }
    for (nid, losers) in &dropped {
        output
            .stderr
            .extend(report_id_collision(nid, &unique[indices[nid]], losers));
    }
    if unique.len() <= 1 {
        output.nodes = unique;
        return Ok((output, edges));
    }
    // Each non-code node's normalised label and its entropy: both passes below
    // read them, and they are computed once, on all cores.
    let label_stats = crate::par::map(&unique, crate::par::workers(None), |node| {
        if is_code(node) {
            return None;
        }
        let key = norm(label(node));
        if key.is_empty() {
            return None;
        }
        Some((key, entropy(field(node, "label").unwrap_or(&Value::Null))))
    });
    let mut groups = Vec::<Vec<usize>>::new();
    let mut norm_indices = FastMap::new();
    for (i, stats) in label_stats.iter().enumerate() {
        let Some((key, _)) = stats else {
            continue;
        };
        let key = key.clone();
        let next = groups.len();
        let g = *norm_indices.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            next
        });
        groups[g].push(i);
    }
    let mut uf = ProtectedUnion::new(&options.protected_ids, UnionFind::default());
    let mut exact_merges = 0;
    for group in &groups {
        if group.len() <= 1 {
            continue;
        }
        let mut files: Vec<Vec<usize>> = Vec::new();
        let mut file_indices = FastMap::new();
        for &i in group {
            let next = files.len();
            let f = *file_indices.entry(source(&unique[i])).or_insert_with(|| {
                files.push(Vec::new());
                next
            });
            files[f].push(i);
        }
        for file_group in files {
            if source(&unique[file_group[0]]).is_empty() {
                continue;
            }
            exact_merges += merge_exact_group(&file_group, &unique, &mut uf);
        }
        let mut mergeable: Vec<_> = group
            .iter()
            .copied()
            .filter(|&i| {
                let n = &unique[i];
                (text(n, "file_type") == "concept" || (anchored(n) && reads_as_file_entity(n)))
                    && !source(n).is_empty()
                    && label_stats[i]
                        .as_ref()
                        .is_some_and(|(_, e)| *e >= ENTROPY_THRESHOLD)
            })
            .collect();
        mergeable.sort_by_key(|&i| id(&unique[i]));
        exact_merges += merge_crossfile_group(&mergeable, &unique, &mut uf);
    }
    let mut candidates = Vec::<usize>::new();
    let mut seen_norms = FastSet::default();
    let mut norms = FastMap::<String, String>::new();
    for (i, (node, stats)) in unique.iter().zip(&label_stats).enumerate() {
        let Some((key, entropy)) = stats else {
            continue;
        };
        if seen_norms.insert(key.clone()) && *entropy >= ENTROPY_THRESHOLD {
            candidates.push(i);
            norms.insert(id(node).into(), key.clone());
        }
    }
    let mut fuzzy_merges = 0;
    if candidates.len() >= 2 {
        let mut lsh = MinHashLsh::new(LSH_THRESHOLD, NUM_PERM);
        let mut sketches = FastMap::new();
        let candidate_indices: FastMap<_, _> =
            candidates.iter().map(|&i| (id(&unique[i]), i)).collect();
        // Sketches are independent of each other: computed on all cores, then
        // indexed in candidate order as before.
        let computed = crate::par::map(&candidates, crate::par::workers(None), |&i| {
            make_minhash(&norms[id(&unique[i])], NUM_PERM)
        });
        for (&i, sketch) in candidates.iter().zip(computed) {
            let nid = id(&unique[i]);
            let _ = lsh.insert(nid, &sketch);
            sketches.insert(nid, sketch);
        }
        for &i in &candidates {
            let node = &unique[i];
            let nid = id(node);
            for neighbor_id in lsh.query(&sketches[nid]) {
                if neighbor_id == nid
                    || uf.uf.find(nid) == uf.uf.find(&neighbor_id)
                    || uf.blocked(nid, &neighbor_id)
                {
                    continue;
                }
                let Some(&j) = candidate_indices.get(neighbor_id.as_str()) else {
                    continue;
                };
                let neighbor = &unique[j];
                let (a, b) = (&norms[nid], &norms[&neighbor_id]);
                let Some(score) = score_pair(node, neighbor, a, b, &options.communities) else {
                    continue;
                };
                if score < MERGE_THRESHOLD {
                    continue;
                }
                if a == b && source(node) != source(neighbor) {
                    continue;
                }
                let winner = if uf.protected.contains(nid) {
                    i
                } else if uf.protected.contains(&neighbor_id) {
                    j
                } else {
                    pick_index(&[i, j], &unique)
                };
                uf.union(id(&unique[winner]), nid);
                uf.union(id(&unique[winner]), &neighbor_id);
                fuzzy_merges += 1;
            }
        }
    }
    if let Some(backend) = &options.dedup_llm_backend {
        let client = client
            .as_mut()
            .ok_or("LLM tiebreak requested without a native provider client")?;
        let candidate_nodes: Vec<_> = candidates.iter().map(|&i| unique[i].clone()).collect();
        llm_tiebreak_inner(
            &candidate_nodes,
            &mut uf,
            &options.communities,
            TiebreakOptions {
                backend,
                batch_size: 30,
                low: 75.0,
                high: 92.0,
            },
            *client,
            &mut output.stdout,
        )?;
    }
    let mut remap = HashMap::new();
    let mut enriched = HashMap::new();
    for (root, members) in uf.uf.components() {
        if members.len() == 1 {
            continue;
        }
        let mut group: Vec<_> = members
            .iter()
            .filter_map(|m| indices.get(m).copied())
            .collect();
        group.sort();
        let protected: Vec<_> = group
            .iter()
            .copied()
            .filter(|&i| uf.protected.contains(id(&unique[i])))
            .collect();
        let winner = if group.is_empty() {
            json!({"id":root})
        } else {
            unique[pick_index(
                if protected.is_empty() {
                    &group
                } else {
                    &protected
                },
                &unique,
            )]
            .clone()
        };
        let winner_id = id(&winner).to_owned();
        let mut merged = winner.clone();
        for &i in &group {
            if id(&unique[i]) != winner_id {
                merged = merge_missing_attributes(&merged, &unique[i]);
            }
        }
        if merged != winner {
            enriched.insert(winner_id.clone(), merged);
        }
        for member in members {
            if member != winner_id {
                remap.insert(member, winner_id.clone());
            }
        }
    }
    if remap.is_empty() {
        output.nodes = unique;
        return Ok((output, edges));
    }
    let mut message = format!("[graphicle] Deduplicated {} node(s)", remap.len());
    let mut parts = Vec::new();
    if exact_merges > 0 {
        parts.push(format!("{exact_merges} exact"));
    }
    if fuzzy_merges > 0 {
        parts.push(format!("{fuzzy_merges} fuzzy"));
    }
    if !parts.is_empty() {
        message.push_str(&format!(" ({})", parts.join(", ")));
    }
    output.stdout.push(format!("{message}."));
    if let Some(hyperedges) = hyperedges.as_mut() {
        remap_hyperedge_members(hyperedges, &remap);
    }
    output.nodes = unique
        .into_iter()
        .filter(|n| !remap.contains_key(id(n)))
        .map(|n| enriched.remove(id(&n)).unwrap_or(n))
        .collect();
    // An edge that is no object fails the run (the output is discarded, so
    // failing before the others are rewritten changes nothing); the rest are
    // rewritten and filtered in place.
    if edges.iter().any(|e| e.record().is_none()) {
        return Err("Expected edge object".into());
    }
    edges.retain_mut(|e| {
        let Some(object) = e.record_mut() else {
            unreachable!("every edge is an object");
        };
        let src = object
            .get("source")
            .or_else(|| object.get("from"))
            .cloned()
            .unwrap_or(Value::Null);
        let tgt = object
            .get("target")
            .or_else(|| object.get("to"))
            .cloned()
            .unwrap_or(Value::Null);
        if src.is_null() || tgt.is_null() {
            return false;
        }
        // Recursive calls and self-referencing FKs survive an unrelated merge.
        // Only a new loop formed by collapsing distinct endpoints is removed.
        let was_self_loop = src == tgt;
        let mapped = |v: Value| match v {
            Value::String(ref s) if remap.contains_key(s) => json!(remap[s]),
            other => other,
        };
        object.put("source", mapped(src));
        object.put("target", mapped(tgt));
        object.take_key("from");
        object.take_key("to");
        object.get("source") != object.get("target") || was_self_loop
    });
    Ok((output, edges))
}
fn llm_tiebreak_inner(
    candidates: &[Value],
    uf: &mut ProtectedUnion,
    communities: &Map<String, Value>,
    context: TiebreakOptions<'_>,
    client: &mut dyn TiebreakClient,
    messages: &mut Vec<String>,
) -> Result<()> {
    let TiebreakOptions {
        backend,
        batch_size,
        low,
        high,
    } = context;

    match client.availability(backend) {
        BackendAvailability::Unknown => {
            messages.push(format!(
                "[graphicle] --dedup-llm: unknown backend {}, skipping LLM tiebreaker.",
                python_compat::repr_string(backend)
            ));
            return Ok(());
        }
        BackendAvailability::MissingCredentials(keys) => {
            messages.push(format!(
                "[graphicle] --dedup-llm: {keys} not set, skipping LLM tiebreaker."
            ));
            return Ok(());
        }
        BackendAvailability::Available => (),
    }
    let mut ambiguous = Vec::new();
    for (i, node) in candidates.iter().enumerate() {
        let a = norm(label(node));
        for (j, neighbor) in candidates.iter().enumerate().skip(i + 1) {
            if uf.uf.find(id(node)) == uf.uf.find(id(neighbor)) {
                continue;
            }
            let b = norm(label(neighbor));
            if score_pair(node, neighbor, &a, &b, communities)
                .is_some_and(|score| score >= low && score < high)
                && !uf.blocked(id(node), id(neighbor))
            {
                ambiguous.push((i, j));
            }
        }
    }
    if ambiguous.is_empty() {
        return Ok(());
    }
    if batch_size == 0 {
        return Err("batch_size must be positive".into());
    }
    for batch in ambiguous.chunks(batch_size) {
        let pairs = batch
            .iter()
            .enumerate()
            .map(|(i, &(a, b))| {
                format!(
                    "{}. \"{}\" vs \"{}\"",
                    i + 1,
                    text(&candidates[a], "label"),
                    text(&candidates[b], "label")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "For each pair below, answer only 'yes' or 'no': are they the same real-world concept?\n\n{pairs}\n\nReply with one line per pair: '1. yes', '2. no', etc."
        );
        match client.call(&prompt, backend, 200) {
            Err(e) => messages.push(format!("[graphicle] --dedup-llm batch failed: {e}")),
            Ok(response) => {
                for line in fsutil::split_lines(response.trim_matches(strings::whitespace)) {
                    let Some((raw, answer)) =
                        line.trim_matches(strings::whitespace).split_once('.')
                    else {
                        continue;
                    };
                    let normalized: String = raw
                        .trim_matches(strings::whitespace)
                        .chars()
                        .map(|c| {
                            let index = crate::decimal_data::DECIMAL
                                .partition_point(|&(lo, _)| lo <= c as u32);
                            if index > 0 && c as u32 <= crate::decimal_data::DECIMAL[index - 1].1 {
                                char::from_u32(
                                    '0' as u32
                                        + (c as u32 - crate::decimal_data::DECIMAL[index - 1].0)
                                            % 10,
                                )
                                .unwrap()
                            } else {
                                c
                            }
                        })
                        .collect();
                    let digits = normalized.strip_prefix(['+', '-']).unwrap_or(&normalized);
                    if digits.is_empty()
                        || digits.starts_with('_')
                        || digits.ends_with('_')
                        || digits.contains("__")
                        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
                    {
                        continue;
                    }
                    let Ok(index) = normalized.replace('_', "").parse::<i64>() else {
                        continue;
                    };
                    if index < 1
                        || index as usize > batch.len()
                        || !answer
                            .trim_matches(strings::whitespace)
                            .to_lowercase()
                            .starts_with("yes")
                    {
                        continue;
                    }
                    let (a, b) = batch[index as usize - 1];
                    if uf.blocked(id(&candidates[a]), id(&candidates[b])) {
                        continue;
                    }
                    let w = if uf.protected.contains(id(&candidates[a])) {
                        a
                    } else if uf.protected.contains(id(&candidates[b])) {
                        b
                    } else {
                        pick_index(&[a, b], candidates)
                    };
                    uf.union(id(&candidates[w]), id(&candidates[a]));
                    uf.union(id(&candidates[w]), id(&candidates[b]));
                }
            }
        }
    }
    Ok(())
}
pub fn llm_tiebreak(
    candidates: &[Value],
    uf: &mut UnionFind,
    communities: &Map<String, Value>,
    context: TiebreakOptions<'_>,
    protected: &HashSet<String>,
    client: &mut dyn TiebreakClient,
) -> Result<Vec<String>> {
    let TiebreakOptions {
        backend,
        batch_size,
        low,
        high,
    } = context;

    let mut protected = ProtectedUnion::new(protected, std::mem::take(uf));
    let mut messages = Vec::new();
    let result = llm_tiebreak_inner(
        candidates,
        &mut protected,
        communities,
        TiebreakOptions {
            backend,
            batch_size,
            low,
            high,
        },
        client,
        &mut messages,
    );
    *uf = protected.uf;
    result.map(|_| messages)
}

#[cfg(test)]
mod tests {
    use super::pick_winner;
    use serde_json::json;

    #[test]
    fn merging_preserves_original_loops_and_drops_only_collapsed_edges() {
        let nodes = vec![
            json!({"id":"walk","label":"walk()","source_file":"tree.py","file_type":"code"}),
            json!({"id":"cache","label":"Cache","source_file":"guide.md","file_type":"concept"}),
            json!({"id":"cache_2","label":"cache","source_file":"guide.md","file_type":"concept"}),
        ];
        let edges = vec![
            json!({"source":"walk","target":"walk","relation":"calls"}),
            json!({"from":"cache_2","to":"cache_2","relation":"references"}),
            json!({"source":"cache_2","target":"cache","relation":"references"}),
        ];
        let result = super::deduplicate_entities(
            &nodes,
            &edges,
            &super::DedupOptions::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            result.edges,
            vec![
                json!({"source":"walk","target":"walk","relation":"calls"}),
                json!({"source":"cache","target":"cache","relation":"references"}),
            ]
        );
    }

    #[test]
    fn located_declaration_beats_richer_sourceless_stub() {
        let real = json!({
            "id": "docs_guide_setup", "label": "Setup", "file_type": "document",
            "source_file": "docs/guide.md", "source_location": "L3",
            "attributes": {"kind": "heading"},
        });
        let stub = json!({
            "id": "setup", "label": "Setup", "file_type": "document", "source_file": "",
            "type": "external", "external": true, "_origin": "semantic",
        });
        for order in [[real.clone(), stub.clone()], [stub.clone(), real.clone()]] {
            assert_eq!(pick_winner(&order).unwrap()["id"], "docs_guide_setup");
        }
        // Richness still decides between two located nodes; a chunk suffix still loses.
        let poor = json!({"id": "a_setup", "label": "Setup", "source_file": "a.md"});
        assert_eq!(
            pick_winner(&[poor.clone(), real.clone()]).unwrap()["id"],
            "docs_guide_setup"
        );
        let chunked =
            json!({"id": "x_c2", "label": "Setup", "source_file": "x.md", "attributes": {"k": 1}});
        assert_eq!(pick_winner(&[chunked, stub]).unwrap()["id"], "setup");
    }
}
