//! Workspace scope.
//!
//! `scope: "workspace"` asks one question of the projects of a workspace that
//! the caller may read: the named project first (when the call names one),
//! then the others in creation order, as `graph_projection::link_workspace`
//! enumerates them, without PR scratch projects, at most [`FANOUT`] of them
//! by default.
//!
//! - **Each project is read exactly as a single-project `context` reads it**
//!   ([`query::project_packet`]): its own authorization (`store::repo`), its
//!   snapshot with connector documents filtered for the principal, its arms
//!   and its ranking and packing under the request's budget.
//! - **The projects' packed windows are fused by rank position**: each
//!   project's first window, then each one's second, and so on. Scores from
//!   different corpora do not compare (their term statistics differ), so the
//!   fusion uses positions, as reciprocal rank fusion does across arms. The
//!   fused list is packed once more under the request's budget, so the packet
//!   keeps one budget whatever the number of projects.
//! - **Every window and memory carries its project**: `repository_id` and
//!   `project` (its name) in `sources`, `evidence` and `memories`, and a
//!   `Project:` line in the packet's text.
//! - **The cross-repository step** follows one hop of the import edges that
//!   `graph.link` recorded between two projects of the scope, in both
//!   directions, through one permission-filtered reader
//!   ([`graph_projection::live_cross_edges`]). A packed window that shows an
//!   import (or names the imported symbol) lifts the imported symbol's window
//!   from the exporting project; a packed window that holds an imported
//!   symbol lifts the importing project's use of it. A lifted window follows
//!   the window that led to it and carries `cross_repo` facts (the edge, both
//!   ends and their stable ids). It is a graph mechanism: `graph_retrieval:
//!   false` turns it off.
//! - **A project the principal may not read takes no part**: it is not
//!   searched, no edge to or from it is followed, and no count includes it.
//! - **A project whose read fails is left out** and listed in `projects`
//!   with `status: "error"` (`workspace.projects_failed` counts them), as a
//!   degraded code arm never fails an interactive query; the named project's
//!   failure fails the read, as a direct read of it would. The step likewise
//!   degrades: it withholds an edge whose episode manifest cannot be read,
//!   and an error in the step leaves it with nothing lifted and the message
//!   in its report.
//!
//! Queries stay read-only: everything here runs in the
//! caller's read-only snapshot.
use crate::{
    Error, Result, code_graph_arm, digest, evidence, graph_projection,
    graph_projection::CrossDirection,
    integer, native, query,
    store::{Principal, Store, repo, rows, s},
};
use graphicle::graph::Graph;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Projects one workspace-scope read searches by default.
pub const FANOUT: usize = 8;
/// The most a call may ask for (`max_projects`).
pub const MAX_FANOUT: usize = 32;
/// Windows the cross-repository step lifts into one packet, at most.
pub const CROSS_LIMIT: usize = 6;
/// Edges the step follows for one packet, at most: the lifts and the
/// windows it finds already in the list.
const FOLLOW_LIMIT: usize = 4 * CROSS_LIMIT;
/// The `cross_repo` facts of a window the cross-repository step reached.
pub const FACTS: &str = "cross_repo";
/// `pack` takes at most this many hits and memories.
const PACK_HITS: usize = 40;
const PACK_MEMORIES: usize = 200;

/// What a recall reads: one project (the default) or the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Project,
    Workspace,
}

impl Scope {
    /// `scope`: absent, null or `"project"`, else `"workspace"`.
    pub fn parse(args: &Value) -> Result<Self> {
        match args.get("scope") {
            None | Some(Value::Null) => Ok(Self::Project),
            Some(value) => match value.as_str() {
                Some("project") => Ok(Self::Project),
                Some("workspace") => Ok(Self::Workspace),
                _ => Err(Error::bad("scope must be project or workspace")),
            },
        }
    }
}

/// The projects a workspace-scope read searches, in order: `anchor` first,
/// authorized as any single-project read is (an unreadable or unknown
/// anchor is refused), then the workspace's other projects that `p` may
/// read, in creation order, without PR scratch projects, at
/// most `cap` in all. Returns their rows and how many readable projects the
/// cap left out. A project `p` may not read is passed over without a trace.
pub fn projects(
    db: &Connection,
    p: &Principal,
    anchor: Option<&str>,
    cap: usize,
) -> Result<(Vec<Value>, usize)> {
    let scratch = graph_projection::pr_scratch_projects(db, &p.workspace)?;
    let mut chosen = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(anchor) = anchor {
        let row = repo(db, p, anchor)?;
        seen.insert(s(&row, "id").to_string());
        chosen.push(row);
    }
    let mut left_out = 0usize;
    for row in rows(
        db,
        "SELECT id FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id",
        [&p.workspace],
    )? {
        let id = s(&row, "id");
        if seen.contains(id) || scratch.contains(id) || !graph_projection::repo_readable(db, p, id)?
        {
            continue;
        }
        if chosen.len() >= cap {
            left_out += 1;
            continue;
        }
        seen.insert(id.to_string());
        chosen.push(repo(db, p, id)?);
    }
    Ok((chosen, left_out))
}

impl Store {
    /// The ids of the projects a workspace read that names none searches by
    /// default, in its order ([`projects`] with [`FANOUT`]):
    /// `graphicle mcp --workspace` builds their waiting graphs at start.
    pub fn workspace_projects(&self, workspace: &str, token: &str) -> Result<Vec<String>> {
        self.authorized_read(workspace, token, "read", |db, p| {
            Ok(projects(db, p, None, FANOUT)?
                .0
                .iter()
                .map(|row| s(row, "id").to_string())
                .collect())
        })
    }
}

/// One project of the scope, read.
struct Read {
    /// Its place in the scope's order.
    position: usize,
    repository: Value,
    snapshot: query::Snapshot,
    packet: Value,
}

impl Read {
    fn id(&self) -> &str {
        s(&self.repository, "id")
    }
    fn name(&self) -> &str {
        s(&self.repository, "name")
    }
    /// The windows its own packet packed, in rank order.
    fn sources(&self) -> &[Value] {
        self.packet["sources"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
    fn document(&self, id: &str) -> Option<&Value> {
        self.snapshot.documents.iter().find(|d| s(d, "id") == id)
    }
}

/// The arguments one project's read takes: the request's, for that project,
/// without the options that only the fused packet applies.
fn project_args(args: &Value, repository: &str) -> Value {
    let mut args = args.clone();
    if let Some(fields) = args.as_object_mut() {
        for key in [
            "scope",
            "max_projects",
            "shown",
            "pack_order",
            "reading_guidance",
        ] {
            fields.shift_remove(key);
        }
    }
    args["repository_id"] = json!(repository);
    args
}

/// `context` with `scope: "workspace"` for `p` (see the module comment).
/// `repository_id`, when given, names the project read first.
pub fn context(
    db: &Connection,
    p: &Principal,
    args: &Value,
    embedding: Option<&crate::model::Profile>,
    reranker: Option<&crate::model::Profile>,
) -> Result<Value> {
    let question = crate::string(args, "question", 16000)?;
    let cap = integer(args, "max_projects", FANOUT, MAX_FANOUT)?.max(1);
    let max_tokens = integer(args, "max_tokens", 3000, 16000)?.max(256);
    let max_chars = integer(args, "max_chars", 12000, 64000)?.max(2000);
    let anchor = match args.get("repository_id") {
        None | Some(Value::Null) => None,
        Some(_) => Some(crate::string(args, "repository_id", 100)?),
    };
    let (repositories, left_out) = projects(db, p, anchor, cap)?;
    let mut reads = Vec::with_capacity(repositories.len());
    // A project whose read fails is left out and reported, as a degraded code
    // arm never fails an interactive query; the named project fails the read,
    // as a direct read of it would.
    let mut failed: Vec<(usize, Value, Error)> = Vec::new();
    for (position, repository) in repositories.into_iter().enumerate() {
        let id = s(&repository, "id").to_string();
        match query::project_packet(
            db,
            p,
            &repository,
            &project_args(args, &id),
            embedding,
            reranker,
        ) {
            Ok((snapshot, packet)) => reads.push(Read {
                position,
                repository,
                snapshot,
                packet,
            }),
            Err(error) if anchor.is_none() || position > 0 => {
                failed.push((position, repository, error));
            }
            Err(error) => return Err(error),
        }
    }
    // No project could be read: the request is at fault, or the store.
    if reads.is_empty() && !failed.is_empty() {
        return Err(failed.swap_remove(0).2);
    }
    // Fusion by rank position: each project's first window, then each one's
    // second, and so on, in scope order.
    let longest = reads.iter().map(|r| r.sources().len()).max().unwrap_or(0);
    let mut order: Vec<(usize, usize)> = Vec::new();
    for rank in 0..longest {
        for (index, read) in reads.iter().enumerate() {
            if rank < read.sources().len() {
                order.push((index, rank));
            }
        }
    }
    let graph_on = args["graph_retrieval"] != false;
    let step = if graph_on {
        // A graph mechanism: like the code arm, it degrades (and says why)
        // rather than failing the query.
        cross_step(db, p, &reads, &order).unwrap_or_else(|error| Step::failed(&error))
    } else {
        Step::off()
    };
    let mut hits = Vec::new();
    let mut reasons = Map::new();
    let mut identities = BTreeSet::new();
    for &(index, rank) in &order {
        let read = &reads[index];
        let reference = &read.sources()[rank];
        // A window the step lifted earlier in the list is not repeated.
        if step.superseded.contains(&(index, rank)) {
            continue;
        }
        let mut hit = fused_hit(read, reference)?;
        if let Some(facts) = step.annotations.get(&(index, rank)) {
            hit["source"][FACTS] = facts.clone();
        }
        let identity = s(&hit, "identity").to_string();
        if identities.insert(identity.clone()) {
            reasons.insert(identity, reason_list(reference));
            hits.push(hit);
        }
        for lift in step.lifts.get(&(index, rank)).into_iter().flatten() {
            let identity = s(lift, "identity").to_string();
            if identities.insert(identity.clone()) {
                reasons.insert(identity, json!([graph_projection::VIA_CROSS_REPO_IMPORT]));
                hits.push(lift.clone());
            }
        }
    }
    hits.truncate(PACK_HITS);
    // Reviewed memories each project's packet included, fused the same way.
    let lists: Vec<Vec<Value>> = reads
        .iter()
        .map(|read| {
            read.packet["memories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|included| {
                    let id = s(included, "id");
                    let full = read.snapshot.memories.iter().find(|m| s(m, "id") == id)?;
                    let mut record = full.clone();
                    record["repository_id"] = json!(read.id());
                    record["project"] = json!(read.name());
                    Some(json!({"identity":format!("{}:memory:{id}", read.id()),"kind":"memory","memory":record}))
                })
                .collect()
        })
        .collect();
    let mut memories = Vec::new();
    let deepest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..deepest {
        for list in &lists {
            if let Some(memory) = list.get(rank) {
                memories.push(memory.clone());
            }
        }
    }
    memories.truncate(PACK_MEMORIES);
    let state = digest(&json!({"scope":"workspace","projects":reads
        .iter()
        .map(|read| json!([read.id(), read.snapshot.state]))
        .collect::<Vec<_>>()}));
    let coverage = merge(
        &reads
            .iter()
            .map(|r| &r.packet["coverage"])
            .collect::<Vec<_>>(),
    );
    let mut request = json!({"repo":{"id":anchor,"name":header(&reads),"snapshot":state},
        "question":question,"hits":hits,"memories":memories,"max_tokens":max_tokens,"max_chars":max_chars,
        "reasons":reasons,"coverage":coverage,"needs":args.get("needs").cloned().unwrap_or(json!([])),
        "semantic":{"loaded":reads.iter().any(|r| r.packet["semantic"]["loaded"] == true)}});
    // The packing options, as `query::context_on` sets them: `pack_focus`
    // takes its recipe default when unset.
    match args.get("pack_focus").filter(|v| !v.is_null()) {
        Some(value) if value.is_object() => request["pack_focus"] = value.clone(),
        Some(value) if value == true => request["pack_focus"] = json!(true),
        Some(_) => {}
        None => {
            if query::recipe_defaults()["pack_focus"] == true {
                request["pack_focus"] = json!(true);
            }
        }
    }
    if args["compact_citations"] == true {
        request["compact_citations"] = json!(true);
    }
    for option in ["pack_order", "shown", "reading_guidance"] {
        if let Some(value) = args.get(option).filter(|v| !v.is_null()) {
            request[option] = value.clone();
        }
    }
    let mut packet = native("pack", request)?;
    let mut evidence = Vec::new();
    for source in packet["sources"].as_array().into_iter().flatten() {
        let read = reads
            .iter()
            .find(|read| read.id() == s(source, "repository_id"))
            .ok_or_else(|| Error::bad("Packed source unavailable"))?;
        let document = read
            .document(s(source, "id"))
            .ok_or_else(|| Error::bad("Packed source unavailable"))?;
        let a = source["byte_start"].as_u64().unwrap_or(0) as usize;
        let b = source["byte_end"].as_u64().unwrap_or(0) as usize;
        let mut window = evidence::from_range(document, a, b)?;
        // The code arm's and the step's facts, and the document links each
        // project's read followed.
        for key in [code_graph_arm::ARM, crate::doc_links::ARM, FACTS] {
            if let Some(facts) = source.get(key) {
                window[key] = facts.clone();
            }
        }
        window["repository_id"] = json!(read.id());
        window["project"] = json!(read.name());
        evidence.push(window);
    }
    let edges_used: BTreeSet<String> = evidence
        .iter()
        .filter_map(|window| window.get(FACTS))
        .map(|facts| facts["edge"]["id"].to_string())
        .collect();
    let searched: Vec<Value> = reads
        .iter()
        .map(|read| project_report(read, &evidence, &packet))
        .collect();
    packet["arms"] = arms(&reads, &packet, &step);
    packet[code_graph_arm::ARM] = code_graph(&searched, &evidence);
    packet[crate::doc_links::ARM] = doc_links(&reads, &evidence);
    // Each project of the scope in its order: searched, or left out with
    // the error that stopped its read.
    let mut projects: Vec<(usize, Value)> = reads
        .iter()
        .map(|read| read.position)
        .zip(searched)
        .collect();
    projects.extend(failed.iter().map(|(position, repository, error)| {
        (
            *position,
            json!({"repository_id":repository["id"],"name":repository["name"],"status":"error","error":error.message}),
        )
    }));
    projects.sort_by_key(|(position, _)| *position);
    let projects: Vec<Value> = projects.into_iter().map(|(_, report)| report).collect();
    let mut mechanisms = merge(
        &reads
            .iter()
            .map(|r| &r.packet["mechanisms"])
            .collect::<Vec<_>>(),
    );
    if let Some(fields) = mechanisms.as_object_mut() {
        fields.insert("graph_episode".into(), Value::Null);
        fields.insert(
            "reviewed_memory_included".into(),
            json!(packet["memories"].as_array().map_or(0, Vec::len)),
        );
        fields.insert("source_windows_included".into(), json!(evidence.len()));
        fields.insert("scope".into(), json!("workspace"));
        fields.insert(
            "workspace".into(),
            json!({"projects_searched":reads.len(),"cross_edges_used":edges_used.len()}),
        );
        // Links used, by the windows this packet holds, as `query::context_on`
        // counts one project's (document links are the only
        // links windows carry).
        let mut rungs: BTreeMap<String, u64> = BTreeMap::new();
        for window in &evidence {
            if let Some(facts) = window.get(crate::doc_links::ARM) {
                *rungs.entry(s(facts, "rung").to_string()).or_default() += 1;
            }
        }
        if let Some(used) = fields.get_mut("links_used").and_then(Value::as_object_mut) {
            used.insert("document".into(), json!(rungs.values().sum::<u64>()));
            used.insert("by_rung".into(), json!(rungs));
        }
    }
    packet["graph_source"] = same(reads.iter().map(|r| &r.packet["graph_source"]));
    packet["evidence"] = json!(evidence);
    packet["mechanisms"] = mechanisms;
    packet["source_state"] = json!(state);
    packet["evidence_strength"] = strength(&reads);
    packet["scope"] = json!("workspace");
    packet["workspace"] = json!({"workspace_id":p.workspace,"projects_searched":reads.len(),
        "projects_failed":failed.len(),"projects_left_out":left_out,"max_projects":cap,"cross_repo":step.report});
    packet["projects"] = json!(projects);
    if let Some(first) = reads.first() {
        for key in ["depth", "arm_weights", "retrieval_defaults", "stemmer"] {
            if let Some(value) = first.packet.get(key) {
                packet[key] = value.clone();
            }
        }
    }
    packet["model_calls"] = json!(
        reads
            .iter()
            .map(|r| r.packet["model_calls"].as_u64().unwrap_or(0))
            .sum::<u64>()
    );
    Ok(packet)
}

/// The fused list's hit for one window a project's own packet packed: the
/// same bytes and reference, with its project.
fn fused_hit(read: &Read, reference: &Value) -> Result<Value> {
    let id = s(reference, "id");
    let document = read
        .document(id)
        .ok_or_else(|| Error::bad("Packed source unavailable"))?;
    let a = reference["byte_start"].as_u64().unwrap_or(0) as usize;
    let b = reference["byte_end"].as_u64().unwrap_or(0) as usize;
    let text = s(document, "content")
        .get(a..b)
        .ok_or_else(|| Error::bad("Invalid packed source coordinates"))?;
    let mut source = reference.as_object().cloned().unwrap_or_default();
    for key in ["reason", "preview", "rank"] {
        source.shift_remove(key);
    }
    source.insert("repository_id".into(), json!(read.id()));
    source.insert("project".into(), json!(read.name()));
    Ok(
        json!({"identity":format!("{}:{id}:{a}", read.id()),"kind":"source","origin":format!("{}:{id}", read.id()),
        "title":document["title"],"text":text,"source":source}),
    )
}

/// The arms that ranked a packed window (`lexical+code_graph`), as `rank`
/// reports reasons.
fn reason_list(reference: &Value) -> Value {
    json!(
        s(reference, "reason")
            .split('+')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
    )
}

/// The packet's opening line names the projects searched.
fn header(reads: &[Read]) -> String {
    let mut names = String::new();
    for read in reads {
        if names.len() > 200 {
            names.push_str(", …");
            break;
        }
        if !names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(read.name());
    }
    match reads.len() {
        1 => format!("1 project of this workspace: {names}"),
        n => format!("{n} projects of this workspace: {names}"),
    }
}

/// One project's part of a workspace packet.
fn project_report(read: &Read, evidence: &[Value], packet: &Value) -> Value {
    let windows: Vec<&Value> = evidence
        .iter()
        .filter(|w| w["repository_id"] == read.id())
        .collect();
    let arm = &read.packet[code_graph_arm::ARM];
    let mut code_graph = json!({"status":arm["status"]});
    for key in [
        "episode",
        "episode_lag",
        "current_episode",
        "projection",
        "anchored",
        "withheld_changed",
        "withheld_stale_edges",
        "error",
    ] {
        if let Some(value) = arm.get(key) {
            code_graph[key] = value.clone();
        }
    }
    code_graph["cited"] = json!(
        windows
            .iter()
            .filter(|w| w.get(code_graph_arm::ARM).is_some())
            .count()
    );
    let mut report = json!({"repository_id":read.id(),"name":read.name(),"status":"searched",
        "snapshot":read.repository["snapshot"],"source_state":read.snapshot.state,
        "graph_source":read.packet["graph_source"],"graph_episode":read.packet["mechanisms"]["graph_episode"],
        "code_graph":code_graph,"coverage":read.packet["coverage"],"evidence_strength":read.packet["evidence_strength"],
        "windows":windows.len(),
        "memories":packet["memories"].as_array().into_iter().flatten().filter(|m| m["repository_id"] == read.id()).count(),
        "freshness":read.packet["freshness"]});
    for key in ["filter", "local_dense"] {
        if let Some(value) = read.packet.get(key) {
            report[key] = value.clone();
        }
    }
    report
}

/// The workspace packet's `doc_links`: the
/// searched projects' own reports merged (counts add up), `followed` when one
/// followed links and none failed (else `counted` when one counted links
/// without following them), the windows that carry link facts, and each
/// project's status. Links between projects are not followed here.
fn doc_links(reads: &[Read], evidence: &[Value]) -> Value {
    let reports: Vec<&Value> = reads
        .iter()
        .map(|read| &read.packet[crate::doc_links::ARM])
        .collect();
    let mut report = merge(&reports);
    // The lift bound is per project: it stays as each project reports it
    // rather than adding up.
    if let Some(limit) = reports.iter().find_map(|report| report.get("limit")) {
        report["limit"] = limit.clone();
    }
    let statuses: Vec<&str> = reports
        .iter()
        .map(|report| report["status"].as_str().unwrap_or(""))
        .collect();
    if statuses.contains(&"followed") && !statuses.contains(&"error") {
        report["status"] = json!("followed");
    } else if statuses.contains(&"counted") && !statuses.contains(&"error") {
        report["status"] = json!("counted");
    } else if statuses.is_empty() {
        report["status"] = json!("not_requested");
    }
    report["cited"] = json!(
        evidence
            .iter()
            .filter(|w| w.get(crate::doc_links::ARM).is_some())
            .count()
    );
    report["projects"] = json!(
        reads
            .iter()
            .map(|read| (
                read.id().to_string(),
                read.packet[crate::doc_links::ARM]["status"].clone()
            ))
            .collect::<Map<_, _>>()
    );
    report
}

/// The workspace packet's `code_graph`: `fused` when every project's arm
/// read its graph (`fused` or `no_match`) and one fused, `no_match` or
/// `disabled` when all did so, else `mixed`; the largest `episode_lag`
/// (null when a project has none); the windows it cited; and each
/// project's status.
fn code_graph(projects: &[Value], evidence: &[Value]) -> Value {
    let statuses: Vec<&str> = projects
        .iter()
        .map(|p| p["code_graph"]["status"].as_str().unwrap_or(""))
        .collect();
    let read = |status: &&str| matches!(*status, "fused" | "no_match");
    let status = if statuses.is_empty() {
        "not_requested"
    } else if statuses.iter().all(|s| *s == "disabled") {
        "disabled"
    } else if statuses.iter().all(|s| *s == "no_match") {
        "no_match"
    } else if statuses.iter().all(read) {
        "fused"
    } else {
        "mixed"
    };
    let lags: Vec<Option<u64>> = projects
        .iter()
        .map(|p| p["code_graph"]["episode_lag"].as_u64())
        .collect();
    let lag = if !lags.is_empty() && lags.iter().all(Option::is_some) {
        json!(lags.iter().flatten().max())
    } else {
        Value::Null
    };
    json!({"status":status,"episode_lag":lag,
        "cited":evidence.iter().filter(|w| w.get(code_graph_arm::ARM).is_some()).count(),
        "projects":projects.iter().map(|p| (s(p,"repository_id").to_string(), p["code_graph"]["status"].clone())).collect::<Map<_,_>>()})
}

/// Per-arm candidates (summed over the projects) and the fused packet's
/// windows each arm ranked; the cross-repository step counts as the arm
/// `cross_repo_import`.
fn arms(reads: &[Read], packet: &Value, step: &Step) -> Value {
    let mut names: Vec<String> = Vec::new();
    let mut candidates: BTreeMap<String, u64> = BTreeMap::new();
    for read in reads {
        for (name, arm) in read.packet["arms"].as_object().into_iter().flatten() {
            if !names.contains(name) {
                names.push(name.clone());
            }
            *candidates.entry(name.clone()).or_default() += arm["candidates"].as_u64().unwrap_or(0);
        }
    }
    let via = graph_projection::VIA_CROSS_REPO_IMPORT.to_string();
    if step.lifted > 0 {
        names.push(via.clone());
        candidates.insert(via, step.lifted as u64);
    }
    let sources = packet["sources"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut report = Map::new();
    for name in names {
        let selected = sources
            .iter()
            .filter(|source| s(source, "reason").split('+').any(|part| part == name))
            .count();
        report.insert(
            name.clone(),
            json!({"candidates":candidates.get(&name).copied().unwrap_or(0),"selected":selected}),
        );
    }
    Value::Object(report)
}

/// `evidence_strength`: weak when every project's evidence is, the best
/// question coverage, and each project's own.
fn strength(reads: &[Read]) -> Value {
    let weak = reads
        .iter()
        .all(|r| r.packet["evidence_strength"]["weak"] != false);
    let coverage = reads
        .iter()
        .filter_map(|r| r.packet["evidence_strength"]["question_coverage"].as_f64())
        .fold(None, |best: Option<f64>, x| {
            Some(best.map_or(x, |b| b.max(x)))
        });
    json!({"weak":weak,"question_coverage":coverage,
        "projects":reads.iter().map(|r| (r.id().to_string(), r.packet["evidence_strength"].clone())).collect::<Map<_,_>>()})
}

/// The value every input has, else `"mixed"` (null without inputs).
fn same<'a>(values: impl Iterator<Item = &'a Value>) -> Value {
    let mut first: Option<&Value> = None;
    for value in values {
        match first {
            None => first = Some(value),
            Some(seen) if seen != value => return json!("mixed"),
            Some(_) => {}
        }
    }
    first.cloned().unwrap_or(Value::Null)
}

/// Field by field over the projects' objects: counts add up, flags are
/// true when one is, equal values stay, different strings read `"mixed"`,
/// and objects merge the same way. Fields that differ otherwise are left
/// out (each project's own value is in its `projects` entry).
fn merge(values: &[&Value]) -> Value {
    let objects: Vec<&Map<String, Value>> = values.iter().filter_map(|v| v.as_object()).collect();
    let mut keys: Vec<&String> = Vec::new();
    for object in &objects {
        for key in object.keys() {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    let mut merged = Map::new();
    for key in keys {
        let present: Vec<&Value> = objects.iter().filter_map(|o| o.get(key)).collect();
        let value = if present.iter().all(|v| v.is_u64()) {
            json!(present.iter().filter_map(|v| v.as_u64()).sum::<u64>())
        } else if present.iter().all(|v| v.is_boolean()) {
            json!(present.iter().any(|v| **v == true))
        } else if present.iter().all(|v| v.is_object()) {
            merge(&present)
        } else if present.iter().all(|v| v.is_string() || v.is_null()) {
            same(present.iter().copied())
        } else if present.windows(2).all(|pair| pair[0] == pair[1]) {
            // Equal lists stay (`doc_links_sources`).
            present[0].clone()
        } else {
            continue;
        };
        merged.insert(key.clone(), value);
    }
    Value::Object(merged)
}

// ---------------------------------------------------------------------------
// The cross-repository step
// ---------------------------------------------------------------------------

/// What the cross-repository step adds to the fused list.
struct Step {
    /// Windows lifted from another project, after the fused window
    /// `(project, rank)` that led to them.
    lifts: BTreeMap<(usize, usize), Vec<Value>>,
    /// `cross_repo` facts for fused windows the step reached that were
    /// already in the list before the window that led to them.
    annotations: BTreeMap<(usize, usize), Value>,
    /// Fused windows a lift replaces: the same code, later in the list.
    superseded: BTreeSet<(usize, usize)>,
    lifted: usize,
    /// `cross_repo` in the packet's `workspace` report.
    report: Value,
}

impl Step {
    fn off() -> Self {
        Self {
            lifts: BTreeMap::new(),
            annotations: BTreeMap::new(),
            superseded: BTreeSet::new(),
            lifted: 0,
            report: json!({"enabled":false}),
        }
    }
    /// Nothing done yet.
    fn on() -> Self {
        Self {
            report: json!({"enabled":true,"edges_read":0,"followed":0,"lifted":0,"annotated":0,
                "withheld_changed":0,"limit":CROSS_LIMIT}),
            ..Self::off()
        }
    }
    /// The step could not run: nothing lifted, and the report says why.
    fn failed(error: &Error) -> Self {
        let mut step = Self::on();
        step.report["error"] = json!(error.message);
        step
    }
}

/// Episode graphs the step reads, loaded once per call; `None` for one that
/// cannot be read (the step then passes over its edges, as the code arm
/// degrades instead of failing the query).
struct Graphs<'a> {
    db: &'a Connection,
    loaded: HashMap<(String, String), Option<Loaded>>,
}

/// A project's fused windows by document: (rank, byte start, byte end).
type Placed<'a> = HashMap<&'a str, Vec<(usize, usize, usize)>>;

struct Loaded {
    handle: graphicle::session::GraphHandle,
    starts: HashMap<String, Vec<usize>>,
}

impl<'a> Graphs<'a> {
    fn new(db: &'a Connection) -> Self {
        Self {
            db,
            loaded: HashMap::new(),
        }
    }
    fn get(&mut self, repository: &str, episode: &str) -> Option<&Loaded> {
        self.loaded
            .entry((repository.to_string(), episode.to_string()))
            .or_insert_with(|| {
                let handle = graph_projection::resident(self.db, repository, episode).ok()?;
                let starts = code_graph_arm::symbol_starts(&handle);
                Some(Loaded { handle, starts })
            })
            .as_ref()
    }
}

/// Whether `word` occurs in `text` as a whole identifier.
fn mentions(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(identifier)
            && !text[at + word.len()..]
                .chars()
                .next()
                .is_some_and(identifier)
    })
}

/// The window the step cites for the other end of an edge: the node itself
/// when it is a symbol; for a module, the first symbol of that file whose
/// text uses the imported name (the importer's use of it), else the
/// module's header. `None` when the node cannot be located in the view.
fn other_end(
    graph: &Graph,
    starts: &HashMap<String, Vec<usize>>,
    node: &str,
    symbol: Option<&str>,
    documents: &HashMap<&str, &Value>,
) -> Option<code_graph_arm::Located> {
    let located = code_graph_arm::locate(graph, starts, node, documents)?;
    if !located.file {
        return Some(located);
    }
    if let Some(symbol) = symbol {
        let content = s(documents.get(located.document.as_str())?, "content");
        if let Some(user) = code_graph_arm::symbols_in(graph, starts, &located.relpath, documents)
            .into_iter()
            .find(|candidate| {
                content
                    .get(candidate.byte_start..candidate.byte_end)
                    .is_some_and(|text| mentions(text, symbol))
            })
        {
            return Some(user);
        }
    }
    Some(located)
}

/// One hop of the recorded import edges between the projects of the scope,
/// from the fused windows in fused order (see the module comment). At most
/// [`CROSS_LIMIT`] windows are lifted. The other end is cited only while its
/// file still has the bytes its episode recorded.
fn cross_step(
    db: &Connection,
    p: &Principal,
    reads: &[Read],
    order: &[(usize, usize)],
) -> Result<Step> {
    let mut step = Step::on();
    if reads.len() < 2 {
        return Ok(step);
    }
    let index: HashMap<&str, usize> = reads
        .iter()
        .enumerate()
        .map(|(i, read)| (read.id(), i))
        .collect();
    // Per project: its outgoing and incoming live edges whose other end is a
    // project of the scope (both ends readable, through the shared reader).
    let mut edges: Vec<[Vec<Value>; 2]> = Vec::with_capacity(reads.len());
    let mut read_count = 0usize;
    for read in reads {
        let mut pair: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
        for (slot, direction, other) in [
            (0, CrossDirection::Outgoing, "target_repository_id"),
            (1, CrossDirection::Incoming, "source_repository_id"),
        ] {
            pair[slot] = graph_projection::live_cross_edges(db, p, read.id(), direction)?
                .into_iter()
                .filter(|edge| index.contains_key(s(edge, other)))
                .collect();
            read_count += pair[slot].len();
        }
        edges.push(pair);
    }
    step.report["edges_read"] = json!(read_count / 2);
    if read_count == 0 {
        return Ok(step);
    }
    let documents: Vec<HashMap<&str, &Value>> = reads
        .iter()
        .map(|read| {
            read.snapshot
                .documents
                .iter()
                .map(|d| (s(d, "id"), d))
                .collect()
        })
        .collect();
    let current: Vec<String> = reads
        .iter()
        .map(|read| graph_projection::current_episode(db, read.id()))
        .collect::<Result<_>>()?;
    // The fused windows of each project, by document: (rank, start, end),
    // and each window's position in the fused list.
    let mut placed: Vec<Placed<'_>> = vec![HashMap::new(); reads.len()];
    let position: HashMap<(usize, usize), usize> = order
        .iter()
        .enumerate()
        .map(|(at, &window)| (window, at))
        .collect();
    for &(i, rank) in order {
        let source = &reads[i].sources()[rank];
        placed[i].entry(s(source, "id")).or_default().push((
            rank,
            source["byte_start"].as_u64().unwrap_or(0) as usize,
            source["byte_end"].as_u64().unwrap_or(0) as usize,
        ));
    }
    let mut graphs = Graphs::new(db);
    let mut followed: BTreeSet<(String, bool)> = BTreeSet::new();
    let mut lifted_at: BTreeSet<(usize, String, usize)> = BTreeSet::new();
    // This project's end of each edge: its file (cheap), then where it is.
    let mut own_files: HashMap<(String, bool), Option<String>> = HashMap::new();
    let mut own_ends: HashMap<(String, bool), Option<code_graph_arm::Located>> = HashMap::new();
    let mut withheld = 0usize;
    for &(i, rank) in order {
        if step.lifted >= CROSS_LIMIT || followed.len() >= FOLLOW_LIMIT {
            break;
        }
        // A window lifted earlier in the list leads nowhere from here: one
        // hop only.
        if step.superseded.contains(&(i, rank)) {
            continue;
        }
        let window = &reads[i].sources()[rank];
        let window_id = s(window, "id");
        let Some(window_path) = window_id.strip_prefix("file:") else {
            continue;
        };
        let (start, end) = (
            window["byte_start"].as_u64().unwrap_or(0) as usize,
            window["byte_end"].as_u64().unwrap_or(0) as usize,
        );
        let line = window["line"].as_u64().unwrap_or(1);
        let end_line = window["end_line"].as_u64().unwrap_or(line);
        let text = documents[i]
            .get(window_id)
            .and_then(|d| s(d, "content").get(start..end))
            .unwrap_or("");
        for (slot, outgoing) in [(0usize, true), (1usize, false)] {
            for edge in &edges[i][slot] {
                if step.lifted >= CROSS_LIMIT || followed.len() >= FOLLOW_LIMIT {
                    break;
                }
                let key = (edge["id"].to_string(), outgoing);
                if followed.contains(&key) {
                    continue;
                }
                let (own, other) = if outgoing {
                    ("source", "target")
                } else {
                    ("target", "source")
                };
                let own_node = s(edge, &format!("{own}_node_id"));
                let own_episode = s(edge, &format!("{own}_episode"));
                let symbol = edge["symbol"].as_str();
                // Does this window show this project's end of the edge?
                let file = own_files.entry(key.clone()).or_insert_with(|| {
                    graphs
                        .get(reads[i].id(), own_episode)
                        .and_then(|g| code_graph_arm::relpath_of(&g.handle, own_node))
                });
                if file.as_deref() != Some(window_path) {
                    continue;
                }
                let Some(own_end) = own_ends
                    .entry(key.clone())
                    .or_insert_with(|| {
                        graphs.get(reads[i].id(), own_episode).and_then(|g| {
                            code_graph_arm::locate(&g.handle, &g.starts, own_node, &documents[i])
                        })
                    })
                    .clone()
                else {
                    continue;
                };
                let overlaps = own_end.byte_start < end && start < own_end.byte_end;
                let shown = if outgoing {
                    edge["line"]
                        .as_u64()
                        .is_some_and(|at| line <= at && at <= end_line)
                        || symbol.is_some_and(|name| mentions(text, name))
                        || (!own_end.file && overlaps)
                } else {
                    overlaps
                };
                if !shown {
                    continue;
                }
                let j = index[s(edge, &format!("{other}_repository_id"))];
                let other_episode = s(edge, &format!("{other}_episode")).to_string();
                let Some(target) = graphs.get(reads[j].id(), &other_episode).and_then(|g| {
                    other_end(
                        &g.handle,
                        &g.starts,
                        s(edge, &format!("{other}_node_id")),
                        symbol,
                        &documents[j],
                    )
                }) else {
                    continue;
                };
                // The other end is cited only from bytes its episode recorded;
                // an episode whose manifest cannot be read (none recorded, or
                // its row pruned while its graph is still cached) is withheld.
                let Some(&document) = documents[j].get(target.document.as_str()) else {
                    continue;
                };
                if other_episode != current[j]
                    && graph_projection::stored_episode_heads(db, reads[j].id(), &other_episode)
                        .ok()
                        .flatten()
                        .is_none_or(|heads| {
                            heads.get(&target.relpath).map(String::as_str)
                                != Some(s(document, "sha256"))
                        })
                {
                    withheld += 1;
                    continue;
                }
                followed.insert(key);
                let facts = facts(
                    edge,
                    outgoing,
                    &reads[i],
                    window_id,
                    &own_end,
                    &reads[j],
                    &target,
                    &other_episode,
                );
                // The other project's own list may hold the same code: before
                // this window, it keeps its place and gains the facts; after
                // it, the lift takes its place here.
                let same = placed[j].get(target.document.as_str()).and_then(|list| {
                    list.iter()
                        .find(|(_, a, b)| *a < target.byte_end && target.byte_start < *b)
                        .map(|(rank_j, _, _)| *rank_j)
                });
                if let Some(rank_j) = same {
                    if position[&(j, rank_j)] < position[&(i, rank)] {
                        step.annotations.entry((j, rank_j)).or_insert(facts);
                        continue;
                    }
                    if !step.superseded.insert((j, rank_j)) {
                        continue;
                    }
                }
                if !lifted_at.insert((j, target.document.clone(), target.byte_start)) {
                    continue;
                }
                step.lifts
                    .entry((i, rank))
                    .or_default()
                    .push(lifted_hit(&reads[j], document, &target, facts)?);
                step.lifted += 1;
            }
        }
    }
    step.report["followed"] = json!(followed.len());
    step.report["lifted"] = json!(step.lifted);
    step.report["annotated"] = json!(step.annotations.len());
    step.report["withheld_changed"] = json!(withheld);
    Ok(step)
}

/// The `cross_repo` facts of a window the step reached: the fused window it
/// came from, the node it cites, and the edge (both ends with their stable
/// ids and episodes).
#[allow(clippy::too_many_arguments)]
fn facts(
    edge: &Value,
    outgoing: bool,
    from: &Read,
    evidence_id: &str,
    from_end: &code_graph_arm::Located,
    to: &Read,
    node: &code_graph_arm::Located,
    episode: &str,
) -> Value {
    let end = |side: &str| {
        json!({"repository_id":edge[format!("{side}_repository_id")],"node_id":edge[format!("{side}_node_id")],
            "stable_id":edge[format!("{side}_stable_id")],"episode":edge[format!("{side}_episode")]})
    };
    let mut cited = node.facts.clone();
    cited["repository_id"] = json!(to.id());
    cited["episode"] = json!(episode);
    json!({"via":graph_projection::VIA_CROSS_REPO_IMPORT,
        // The cited node is imported by the window it came from, or imports it.
        "direction":if outgoing { "imported_by" } else { "imports" },
        "from":{"repository_id":from.id(),"project":from.name(),"evidence_id":evidence_id,
            "node_id":from_end.facts["node_id"],"stable_id":from_end.facts["stable_id"]},
        "node":cited,
        "edge":{"id":edge["id"],"relation":edge["relation"],"confidence":edge["confidence"],
            "derivation":edge["derivation"],"module_path":edge["module_path"],"symbol":edge["symbol"],
            "line":edge["line"],"version_status":edge["version_status"],
            "source":end("source"),"target":end("target")}})
}

/// A fused-list hit for a window the step lifted from `read`.
fn lifted_hit(
    read: &Read,
    document: &Value,
    target: &code_graph_arm::Located,
    facts: Value,
) -> Result<Value> {
    let (a, b) = (target.byte_start, target.byte_end);
    let content = s(document, "content");
    let text = content
        .get(a..b)
        .ok_or_else(|| Error::bad("Invalid cross-repository window"))?;
    let reference = evidence::from_range(document, a, b)?;
    let mut source = document.as_object().cloned().unwrap_or_default();
    source.shift_remove("content");
    source.insert("byte_start".into(), json!(a));
    source.insert("byte_end".into(), json!(b));
    source.insert("line".into(), json!(target.line));
    source.insert("end_line".into(), json!(target.end_line));
    for key in ["total_bytes", "next_byte", "source_context"] {
        if let Some(value) = reference.get(key) {
            source.insert(key.into(), value.clone());
        }
    }
    source.insert("complete".into(), json!(a == 0 && b == content.len()));
    source.insert("truncated".into(), json!(b < content.len()));
    source.insert(FACTS.into(), facts);
    source.insert("repository_id".into(), json!(read.id()));
    source.insert("project".into(), json!(read.name()));
    Ok(
        json!({"identity":format!("{}:{}:{a}#{FACTS}", read.id(), target.document),
        "kind":"source","origin":format!("{}:{}", read.id(), target.document),
        "title":document["title"],"text":text,"source":source}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_project_unless_it_says_workspace() {
        assert_eq!(Scope::parse(&json!({})).unwrap(), Scope::Project);
        assert_eq!(
            Scope::parse(&json!({"scope":null})).unwrap(),
            Scope::Project
        );
        assert_eq!(
            Scope::parse(&json!({"scope":"project"})).unwrap(),
            Scope::Project
        );
        assert_eq!(
            Scope::parse(&json!({"scope":"workspace"})).unwrap(),
            Scope::Workspace
        );
        for bad in [json!("all"), json!(true), json!(["workspace"])] {
            assert!(Scope::parse(&json!({ "scope": bad })).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_word_is_mentioned_only_as_a_whole_identifier() {
        assert!(mentions("return quantize_cents(total)", "quantize_cents"));
        assert!(mentions("quantize_cents", "quantize_cents"));
        assert!(!mentions(
            "return quantize_cents_v2(total)",
            "quantize_cents"
        ));
        assert!(!mentions("requantize_cents()", "quantize_cents"));
        assert!(!mentions("anything", ""));
    }

    #[test]
    fn merging_adds_counts_ors_flags_and_marks_different_strings() {
        let a = json!({"n":1,"on":false,"kind":"episode","nested":{"by_file":1,"by_symbol":0},"list":[1],"same":["x"],"none":null});
        let b = json!({"n":2,"on":true,"kind":"legacy","nested":{"by_file":2,"by_symbol":0},"list":[2],"same":["x"],"none":null,"only":"b"});
        assert_eq!(
            merge(&[&a, &b]),
            json!({"n":3,"on":true,"kind":"mixed","nested":{"by_file":3,"by_symbol":0},"same":["x"],"none":null,"only":"b"})
        );
        assert_eq!(merge(&[]), json!({}));
        assert_eq!(same([json!("x"), json!("x")].iter()), json!("x"));
        assert_eq!(same(std::iter::empty()), Value::Null);
    }
}
