//! Reviewed memory and code symbols, read in both directions.
//!
//! A memory's links to code symbols are its `data.symbols` and the quotes
//! pinned after it was written (`gf_symbol_pins`; see [`crate::symbols`]).
//! This module reads them the other way round, without a table of its own:
//!
//! - **Back-links** ([`attach`]): for the code symbols a graph tool's
//!   `get_node` answer names, the current reviewed memories that link them
//!   and the documents the reader may read that mention them (`linked`). A
//!   connection to one project shows only that project's ([`confine`]).
//! - **The links step of a query** ([`lift`]), switched by `symbol_links`:
//!   after the fusion, each selected code window lifts the current reviewed
//!   memories linked to a symbol inside it (`code_to_memory`), and each
//!   selected memory lifts the current windows of the symbols it links
//!   (`memory_to_code`). One hop: a lifted item lifts nothing. At most
//!   [`MEMORY_LIMIT`] memories and [`WINDOW_LIMIT`] windows, after the fused
//!   results, the code arm's anchors and the document links' windows, never
//!   one already there. A link counts only while the live-symbol table
//!   describes the bytes the reader reads (the file's sha256) and the symbol
//!   keeps the revision the link recorded, and only memories the query may
//!   serve are lifted (reviewed, current, allowed by its filters, within the
//!   recall bound).
//!
//! The `symbol_links` switch: left out, the links of the selected items are
//! counted (`withheld`) and nothing is lifted, since whether lifted items
//! help answers has not been measured; `true` follows them; `false` reads
//! none. A store whose memories carry no code links reads nothing here, so
//! its packets keep their evidence. A query only reads.
use crate::{
    Error, Result,
    query::Snapshot,
    store::{Principal, rows, s},
    symbols::{self, Live},
};
use graphicle_memory::graph::span;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The switch, the packet field and the facts of the links followed.
pub const ARM: &str = "symbol_links";
/// Memories one query lifts through code windows.
pub const MEMORY_LIMIT: usize = 4;
/// Code windows one query lifts through memories, as the document links'
/// `LIFT_LIMIT` bounds theirs.
pub const WINDOW_LIMIT: usize = 6;
/// Memories and documents a back-link listing names per symbol.
pub const BACK_LINK_LIMIT: usize = 20;

/// The `symbol_links` switch of a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Left out: the links of the selected items are counted, not followed.
    Counted,
    /// `true`: followed one hop.
    Follow,
    /// `false`: no link is read.
    Off,
}

impl Mode {
    pub fn parse(args: &Value) -> Result<Self> {
        match args.get(ARM) {
            None | Some(Value::Null) => Ok(Self::Counted),
            Some(Value::Bool(true)) => Ok(Self::Follow),
            Some(Value::Bool(false)) => Ok(Self::Off),
            Some(_) => Err(Error::bad("symbol_links must be true or false")),
        }
    }
    /// Whether links are followed (lifted into the packet).
    pub fn enabled(self) -> bool {
        self == Self::Follow
    }
}

fn resolved(entries: &[Value]) -> impl Iterator<Item = &Value> {
    entries
        .iter()
        .filter(|entry| entry["state"] == "resolved" && entry["stable_id"].is_string())
}

/// The resolved links of each memory that has code links: its own
/// (`data.symbols`) and its pinned quotes', one per `stable_id` (the first
/// recorded).
fn links_of<'m>(
    db: &Connection,
    repository: &str,
    memories: impl Iterator<Item = &'m Value>,
) -> Result<HashMap<String, Vec<Value>>> {
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for memory in memories {
        let entries = memory["data"]["symbols"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if !entries.is_empty() {
            out.entry(s(memory, "id").to_string())
                .or_default()
                .extend(resolved(entries).cloned());
        }
    }
    if out.is_empty() {
        return Ok(out);
    }
    let ids: BTreeSet<&String> = out.keys().collect();
    for row in rows(
        db,
        "SELECT memory_id,symbols FROM gf_symbol_pins WHERE repository_id=?1 AND state='pinned' \
         AND memory_id IN (SELECT value FROM json_each(?2)) ORDER BY memory_id,relpath,byte_start",
        params![repository, json!(ids).to_string()],
    )? {
        let pinned: Vec<Value> = serde_json::from_str(s(&row, "symbols")).unwrap_or_default();
        if let Some(links) = out.get_mut(s(&row, "memory_id")) {
            links.extend(resolved(&pinned).cloned());
        }
    }
    for links in out.values_mut() {
        let mut seen = BTreeSet::new();
        links.retain(|link| seen.insert(s(link, "stable_id").to_string()));
    }
    out.retain(|_, links| !links.is_empty());
    Ok(out)
}

/// What the `symbol_links` step added to a query.
pub struct Lifted {
    /// Code windows, as candidates no passage covers.
    pub windows: Vec<Value>,
    /// Memory rows as `rank` takes them, marked `via: link`, the link
    /// followed under `link`.
    pub memories: Vec<Value>,
    pub report: Value,
}

#[derive(Default)]
struct Counts {
    considered: usize,
    withheld: usize,
    covered: usize,
    pending: usize,
    changed: usize,
    unavailable: usize,
    over_limit: usize,
}

/// One file's live symbols and line starts, when the table describes the
/// bytes the reader reads.
struct File<'a> {
    document: &'a Value,
    live: Vec<Live>,
    starts: Vec<usize>,
}

/// The links step of a query (see the module documentation). `rows` are the
/// query's candidate rows (its memory rows are the memories it may serve),
/// `sources` and `selected_memories` what the fusion selected, in rank
/// order, and `candidates` the windows already taken.
#[allow(clippy::too_many_arguments)]
pub fn lift(
    db: &Connection,
    repository: &str,
    mode: Mode,
    snapshot: &Snapshot,
    rows: &[Value],
    sources: &[Value],
    selected_memories: &[Value],
    candidates: &[Value],
) -> Result<Lifted> {
    let servable: Vec<&Value> = rows.iter().filter(|row| row["kind"] == "memory").collect();
    let links = if mode == Mode::Off {
        HashMap::new()
    } else {
        links_of(db, repository, servable.iter().map(|row| &row["memory"]))?
    };
    if links.is_empty() {
        return Ok(Lifted {
            windows: Vec::new(),
            memories: Vec::new(),
            report: json!({"status": if mode == Mode::Off { "disabled" } else { "no_links" }}),
        });
    }
    let follow = mode.enabled();
    let documents: HashMap<&str, &Value> =
        snapshot.documents.iter().map(|d| (s(d, "id"), d)).collect();
    // stable_id → the servable memories linking it, in recall order.
    let mut index: HashMap<&str, Vec<(&Value, &Value)>> = HashMap::new();
    for row in servable.iter().copied() {
        for link in links.get(s(&row["memory"], "id")).into_iter().flatten() {
            index
                .entry(s(link, "stable_id"))
                .or_default()
                .push((row, link));
        }
    }
    let mut counts = Counts::default();
    // A code window: the memories linked to a symbol inside it.
    let mut taken: BTreeSet<String> = selected_memories
        .iter()
        .map(|row| s(row, "identity").to_string())
        .collect();
    let mut memories = Vec::new();
    let mut files: HashMap<String, Option<File<'_>>> = HashMap::new();
    for hit in sources {
        let source = &hit["source"];
        let id = s(source, "id");
        let (Some(relpath), Some(a), Some(b)) = (
            id.strip_prefix("file:"),
            source["byte_start"].as_u64(),
            source["byte_end"].as_u64(),
        ) else {
            continue;
        };
        let (a, b) = (a as usize, b as usize);
        if !files.contains_key(relpath) {
            let file = match documents.get(id) {
                Some(document) => Some(File {
                    document,
                    live: symbols::in_file(db, repository, relpath)?,
                    starts: span::line_starts(s(document, "content")),
                }),
                None => None,
            };
            files.insert(relpath.to_string(), file);
        }
        let Some(file) = files[relpath].as_ref() else {
            continue;
        };
        let sha = s(file.document, "sha256");
        let length = s(file.document, "content").len();
        for live in &file.live {
            let Some(linked) = index.get(live.stable_id.as_str()) else {
                continue;
            };
            // The table describes other bytes of this file: wait for the
            // projection.
            if live.sha256 != sha {
                counts.pending += linked.len();
                continue;
            }
            let range = live.range(&file.starts, length);
            if !(range.start < b && a < range.end) {
                continue;
            }
            for (row, link) in linked {
                counts.considered += 1;
                if s(link, "revision") != live.revision {
                    counts.changed += 1;
                    continue;
                }
                // Already selected, or lifted through another window.
                if taken.contains(s(row, "identity")) {
                    counts.covered += 1;
                    continue;
                }
                if !follow {
                    counts.withheld += 1;
                    continue;
                }
                taken.insert(s(row, "identity").to_string());
                if memories.len() >= MEMORY_LIMIT {
                    counts.over_limit += 1;
                    continue;
                }
                // `link`, not `symbol_links`: a memory record with links
                // already carries its own `symbol_links` summary.
                let mut lifted = (*row).clone();
                lifted["memory"]["via"] = json!("link");
                lifted["memory"]["link"] = json!({"direction":"code_to_memory","from":hit["identity"],
                    "stable_id":live.stable_id,"revision":live.revision,
                    "qualified_name":live.identity.qualified_name,"rung":link["rung"]});
                memories.push(lifted);
            }
        }
    }
    // A memory: the current windows of the symbols it links.
    let mut ranges: Vec<(String, usize, usize)> = candidates
        .iter()
        .filter_map(|c| {
            Some((
                s(&c["source"], "id").to_string(),
                c["source"]["byte_start"].as_u64()? as usize,
                c["source"]["byte_end"].as_u64()? as usize,
            ))
        })
        .collect();
    let wanted: BTreeSet<String> = selected_memories
        .iter()
        .filter_map(|row| links.get(s(&row["memory"], "id")))
        .flatten()
        .map(|link| s(link, "stable_id").to_string())
        .collect();
    let live: BTreeMap<String, Live> = symbols::by_ids(db, repository, &wanted)?
        .into_iter()
        .map(|row| (row.stable_id.clone(), row))
        .collect();
    let mut windows = Vec::new();
    for row in selected_memories {
        for link in links.get(s(&row["memory"], "id")).into_iter().flatten() {
            counts.considered += 1;
            let Some(symbol) = live.get(s(link, "stable_id")) else {
                counts.changed += 1;
                continue;
            };
            if symbol.revision != s(link, "revision") {
                counts.changed += 1;
                continue;
            }
            let id = format!("file:{}", symbol.identity.relpath);
            let Some(document) = documents.get(id.as_str()) else {
                counts.unavailable += 1;
                continue;
            };
            if s(document, "sha256") != symbol.sha256 {
                counts.pending += 1;
                continue;
            }
            let content = s(document, "content");
            let range = symbol.range(&span::line_starts(content), content.len());
            // A window already taken covers it.
            if ranges
                .iter()
                .any(|(other, x, y)| *other == id && *x < range.end && range.start < *y)
            {
                counts.covered += 1;
                continue;
            }
            if !follow {
                counts.withheld += 1;
                continue;
            }
            if windows.len() >= WINDOW_LIMIT {
                counts.over_limit += 1;
                continue;
            }
            let facts = json!({"via":"link","direction":"memory_to_code","from":row["identity"],
                "stable_id":symbol.stable_id,"revision":symbol.revision,
                "qualified_name":symbol.identity.qualified_name,"rung":link["rung"]});
            let identity = format!("{id}:{}#{ARM}", range.start);
            if let Some(window) = crate::doc_links::window_with(
                ARM,
                document,
                range.start,
                range.end,
                identity,
                facts,
            ) {
                ranges.push((id, range.start, range.end));
                windows.push(window);
            }
        }
    }
    let report = json!({"status": if follow { "followed" } else { "counted" },
        "considered":counts.considered,"lifted_memories":memories.len(),"lifted_windows":windows.len(),
        "withheld":counts.withheld,"covered":counts.covered,"pending":counts.pending,
        "changed":counts.changed,"unavailable":counts.unavailable,"over_limit":counts.over_limit});
    Ok(Lifted {
        windows,
        memories,
        report,
    })
}

/// Back-links: each node fact (a graph tool's citation) with a
/// `stable_id` gains `linked`, the current reviewed memories of the project
/// that link that symbol (`memories`: id, kind, title, the memory's
/// revision, and the link's rung and recorded symbol revision) and the
/// documents the reader may read that mention it (`documents`, as
/// `graph.doc_links` lists them), at most [`BACK_LINK_LIMIT`] of each. A
/// memory the reader's view withholds (stale, a draft, retired) is never
/// listed. Read-only.
pub fn attach(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    nodes: &mut [Value],
) -> Result<()> {
    let project = s(repository, "id");
    let ids: BTreeSet<String> = nodes
        .iter()
        .filter_map(|node| node["stable_id"].as_str())
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    // Which reviewed memories name these symbols at all: only then is the
    // reader's view of the project's memories read.
    let named: BTreeSet<String> = rows(
        db,
        "SELECT m.id AS id FROM repo_memories m, json_each(m.data,'$.symbols') j \
         WHERE m.repository_id=?1 AND m.state='reviewed' \
         AND json_extract(j.value,'$.stable_id') IN (SELECT value FROM json_each(?2)) \
         UNION SELECT g.memory_id AS id FROM gf_symbol_pins g, json_each(g.symbols) j \
         WHERE g.repository_id=?1 AND g.state='pinned' \
         AND json_extract(j.value,'$.stable_id') IN (SELECT value FROM json_each(?2))",
        params![project, json!(ids).to_string()],
    )?
    .iter()
    .map(|row| s(row, "id").to_string())
    .collect();
    // One view of what the reader may read for every node: each project's
    // documents are listed at most once per call (the project's own once,
    // shared with its memory listing).
    let mut naming = crate::doc_links::Naming::new(db, p);
    let memories: Vec<Value> = if named.is_empty() {
        Vec::new()
    } else {
        let documents = crate::memory::documents(db, repository, p)?;
        naming = naming.knowing(project, &documents);
        crate::memory::list_of(db, repository, &documents, false)?
            .into_iter()
            .filter(|m| {
                named.contains(s(m, "id")) && m["state"] == "reviewed" && m["stale"] != true
            })
            .collect()
    };
    let links = links_of(db, project, memories.iter())?;
    for node in nodes.iter_mut() {
        let Some(stable) = node["stable_id"].as_str().map(str::to_string) else {
            continue;
        };
        let listed: Vec<Value> = memories
            .iter()
            .filter_map(|memory| {
                let link = links
                    .get(s(memory, "id"))?
                    .iter()
                    .find(|link| s(link, "stable_id") == stable)?;
                Some(json!({"id":memory["id"],"kind":memory["kind"],"title":memory["title"],
                    "revision":memory["revision"],"rung":link["rung"],"symbol_revision":link["revision"]}))
            })
            .take(BACK_LINK_LIMIT)
            .collect();
        let documents = naming.documents(project, &stable, BACK_LINK_LIMIT)?;
        node["linked"] = json!({"memories":listed,"documents":documents});
    }
    Ok(())
}

/// Back-links as a connection to one project shows them: such a connection
/// keeps memory and documents on its project (`own`), while a graph tool's
/// `repo` may read another project's code graph. A graph tool's `reply`
/// keeps the reviewed memories of its nodes only when it read `own`'s graph,
/// and only `own`'s documents; the citations text, built from the reply,
/// follows.
pub fn confine(reply: &mut Value, own: &str) {
    let own_graph = reply["repo"] == own;
    for node in reply["nodes"].as_array_mut().into_iter().flatten() {
        let Some(linked) = node.get_mut("linked").and_then(Value::as_object_mut) else {
            continue;
        };
        if !own_graph {
            linked.insert("memories".into(), json!([]));
        }
        if let Some(documents) = linked.get_mut("documents").and_then(Value::as_array_mut) {
            documents.retain(|document| document["repository_id"] == own);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_counts_when_left_out_follows_when_true_and_reads_none_when_false() {
        assert_eq!(Mode::parse(&json!({})).unwrap(), Mode::Counted);
        assert_eq!(
            Mode::parse(&json!({"symbol_links":null})).unwrap(),
            Mode::Counted
        );
        assert_eq!(
            Mode::parse(&json!({"symbol_links":true})).unwrap(),
            Mode::Follow
        );
        assert_eq!(
            Mode::parse(&json!({"symbol_links":false})).unwrap(),
            Mode::Off
        );
        assert!(Mode::parse(&json!({"symbol_links":"yes"})).is_err());
        assert!(Mode::Follow.enabled());
        assert!(!Mode::Counted.enabled() && !Mode::Off.enabled());
    }

    #[test]
    fn a_project_connection_keeps_back_links_on_its_project() {
        let linked = || {
            json!({"memories":[{"id":"m1"}],"documents":[
                {"repository_id":"own","document_id":"file:a.md"},
                {"repository_id":"other","document_id":"file:b.md"}]})
        };
        let reply = |repo: &str| json!({"repo":repo,"nodes":[{"stable_id":"s1","linked":linked()},{"stable_id":"s2"}]});
        // The connection's own graph: its memories stay, other projects'
        // documents go.
        let mut own = reply("own");
        confine(&mut own, "own");
        assert_eq!(own["nodes"][0]["linked"]["memories"], json!([{"id":"m1"}]));
        assert_eq!(
            own["nodes"][0]["linked"]["documents"],
            json!([{"repository_id":"own","document_id":"file:a.md"}])
        );
        assert!(own["nodes"][1].get("linked").is_none());
        // Another project's graph (`repo`): no memory, its own documents only.
        let mut other = reply("other");
        confine(&mut other, "own");
        assert_eq!(other["nodes"][0]["linked"]["memories"], json!([]));
        assert_eq!(
            other["nodes"][0]["linked"]["documents"],
            json!([{"repository_id":"own","document_id":"file:a.md"}])
        );
    }

    #[test]
    fn only_resolved_links_with_a_stable_id_count() {
        let entries = vec![
            json!({"state":"resolved","stable_id":"s1","rung":"named"}),
            json!({"state":"pending","relpath":"a.py"}),
            json!({"state":"resolved"}),
        ];
        let found: Vec<&str> = resolved(&entries)
            .map(|entry| s(entry, "stable_id"))
            .collect();
        assert_eq!(found, ["s1"]);
    }
}
