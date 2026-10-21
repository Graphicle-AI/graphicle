//! The workspace map (`graph.map`; MCP `workspace_map` on workspace
//! connections): the projects of a workspace that the caller may read, the
//! import edges `graph.link` recorded between them, and what each project
//! holds.
//!
//! - **Projects** are the ones `list_projects` lists, in its order (creation
//!   order), without PR scratch projects unless `include_scratch`: `limit` of
//!   them ([`PAGE`] by default, at most [`MAX_PAGE`]: a page reads each of
//!   its projects' documents, as many as one workspace recall reads) from
//!   `offset`. Each is
//!   authorized exactly as a single-project read of it is (`store::repo`). A
//!   project the caller may not read is not listed, and no edge or count
//!   includes it.
//! - **Counts per project** are what the caller's own single-project reads
//!   of it show: its files and the connector documents the caller may read
//!   now (`memory::documents`, as `repository_files` and recall read them),
//!   the code symbols of its projected graph (`gf_symbol_heads`, with the
//!   file set they were projected from and whether that is the current one),
//!   the memories `project_memory` lists, and its links: import edges out of
//!   it and into it (both ends in the map), its documents' mentions linked to
//!   code as `graph.doc_links` counts them for the caller, and the code
//!   symbols its listed memories are anchored to.
//! - **Imports**: one entry per pair of projects in which a project of the
//!   page imports from another project of the map (the edges' source is on
//!   the page), with the number of live edges and the modules they import,
//!   read through the permission-filtered reader that `graph.cross_edges`
//!   and workspace recall share ([`graph_projection::live_cross_edges`]).
//!   Paging through the map lists each pair once.
//! - **A project whose counts cannot be read** is listed with
//!   `status: "error"` and no counts, as workspace recall reports a project
//!   whose read fails; the other projects are mapped.
//!
//! Read-only: everything runs in the caller's read-only snapshot.
use crate::{
    Error, Result, doc_links, graph_projection,
    graph_projection::CrossDirection,
    integer, memory,
    store::{Principal, Store, one, repo, rows, s},
    symbols,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Projects one page of the map holds by default: as many as one workspace
/// recall searches by default ([`crate::workspace::FANOUT`]), since mapping
/// a project reads its documents as recalling from it does.
pub const PAGE: usize = crate::workspace::FANOUT;
/// The most one page may hold (`limit`): the most one workspace recall may
/// search ([`crate::workspace::MAX_FANOUT`]).
pub const MAX_PAGE: usize = crate::workspace::MAX_FANOUT;
/// Modules one import entry names, at most (`modules_total` counts them all).
pub const MODULES: usize = 20;

impl Store {
    /// `graph.map` (see the module comment).
    pub fn workspace_map(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        if args
            .get("include_scratch")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(Error::bad("include_scratch must be true or false"));
        }
        let include_scratch = args["include_scratch"] == true;
        let offset = integer(args, "offset", 0, 100000)?;
        let limit = integer(args, "limit", PAGE, MAX_PAGE)?.max(1);
        self.authorized_read(workspace, token, "read", |db, p| {
            map(db, p, include_scratch, offset, limit)
        })
    }
}

/// The live import edges from one project of the map to another, by pair.
#[derive(Default)]
struct Pair {
    edges: usize,
    modules: BTreeSet<String>,
}

fn map(
    db: &Connection,
    p: &Principal,
    include_scratch: bool,
    offset: usize,
    limit: usize,
) -> Result<Value> {
    let scratch = graph_projection::pr_scratch_projects(db, &p.workspace)?;
    // The map's projects, each authorized as a direct read of it is.
    let mut listed: Vec<Value> = Vec::new();
    for row in rows(
        db,
        "SELECT id FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id",
        [&p.workspace],
    )? {
        let id = s(&row, "id");
        if (scratch.contains(id) && !include_scratch)
            || !graph_projection::repo_readable(db, p, id)?
        {
            continue;
        }
        listed.push(repo(db, p, id)?);
    }
    let members: BTreeSet<&str> = listed.iter().map(|row| s(row, "id")).collect();
    let page: BTreeSet<&str> = listed
        .iter()
        .skip(offset)
        .take(limit)
        .map(|row| s(row, "id"))
        .collect();
    // Every live import edge between two projects of the map: the counts of
    // each end, and the pairs whose source is on the page.
    let mut pairs: BTreeMap<(&str, String), Pair> = BTreeMap::new();
    let mut outgoing: BTreeMap<&str, usize> = BTreeMap::new();
    let mut incoming: BTreeMap<String, usize> = BTreeMap::new();
    let mut edges = 0usize;
    for &source in &members {
        for edge in graph_projection::live_cross_edges(db, p, source, CrossDirection::Outgoing)? {
            let target = s(&edge, "target_repository_id");
            if target == source || !members.contains(target) {
                continue;
            }
            edges += 1;
            *outgoing.entry(source).or_default() += 1;
            *incoming.entry(target.to_string()).or_default() += 1;
            if page.contains(source) {
                let pair = pairs.entry((source, target.to_string())).or_default();
                pair.edges += 1;
                let module = s(&edge, "module_path");
                if !module.is_empty() {
                    pair.modules.insert(module.to_string());
                }
            }
        }
    }
    let mut projects = Vec::new();
    for repository in listed.iter().skip(offset).take(limit) {
        let id = s(repository, "id");
        let mut project = json!({"id":id,"name":repository["name"],"snapshot":repository["snapshot"],
            "created_at":repository["created_at"],"updated_at":repository["updated_at"]});
        if scratch.contains(id) {
            project["role"] = json!(graph_projection::PR_SCRATCH);
            if let Some(of) = crate::folders::scratch_of(db, id)? {
                project["scratch_of"] = json!(of);
            }
        }
        match counts(db, p, repository) {
            Ok(mut counts) => {
                counts["links"]["imports_out"] = json!(outgoing.get(id).copied().unwrap_or(0));
                counts["links"]["imports_in"] = json!(incoming.get(id).copied().unwrap_or(0));
                project["graph"] = graph(db, id)?;
                project["counts"] = counts;
                project["status"] = json!("ok");
            }
            Err(error) => {
                project["status"] = json!("error");
                project["error"] = json!(error.message);
            }
        }
        projects.push(project);
    }
    let imports: Vec<Value> = pairs
        .into_iter()
        .map(|((source, target), pair)| {
            json!({"source_repository_id":source,"target_repository_id":target,"edges":pair.edges,
                "modules":pair.modules.iter().take(MODULES).collect::<Vec<_>>(),
                "modules_total":pair.modules.len()})
        })
        .collect();
    let total = listed.len();
    Ok(
        json!({"workspace_id":p.workspace,"projects":projects,"imports":imports,
        "import_edges":edges,"total":total,
        "next_offset":if offset + limit < total { Some(offset + limit) } else { None }}),
    )
}

/// The file set the project's symbols were projected from, and whether it is
/// the project's current one (otherwise its graph waits for a projection).
fn graph(db: &Connection, id: &str) -> Result<Value> {
    Ok(match symbols::table_episode(db, id)? {
        Some(episode) => {
            let current = episode == graph_projection::current_episode(db, id)?;
            json!({"episode":episode,"current":current})
        }
        None => json!({"episode":null,"current":false}),
    })
}

/// What one project holds, as the caller's own reads of it show (see the
/// module comment); the import counts are added by the caller.
fn counts(db: &Connection, p: &Principal, repository: &Value) -> Result<Value> {
    let id = s(repository, "id");
    let documents = memory::documents(db, repository, p)?;
    let files = documents
        .iter()
        .filter(|d| d["source_kind"] == "repository_file")
        .count();
    let memories = memory::list_of(db, repository, &documents, false)?;
    let memory_symbols: u64 = memories
        .iter()
        .map(|m| m["symbol_links"]["links"].as_u64().unwrap_or(0))
        .sum();
    let symbols = one(
        db,
        "SELECT COUNT(*) AS n FROM gf_symbol_heads WHERE repository_id=?",
        [id],
    )?
    .and_then(|row| row["n"].as_u64())
    .unwrap_or(0);
    let mentions = doc_links::linked_mentions(db, p, id, &documents)?;
    Ok(
        json!({"files":files,"documents":documents.len() - files,"symbols":symbols,
        "memories":memories.len(),
        "links":{"document_mentions":mentions,"memory_symbols":memory_symbols}}),
    )
}
