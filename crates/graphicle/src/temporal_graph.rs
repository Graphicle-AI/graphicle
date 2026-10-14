//! Repository-scoped, append-only valid/recorded-time graph assertions.
use crate::{
    Error, Result, graph_projection,
    store::{one, rows, s},
};
use chrono::{DateTime, SecondsFormat, Utc};
use graphicle::graph::Graph;
use graphicle_memory::graph::canonical::AliasSet;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn instant(value: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.timestamp_micros())
        .map_err(|_| Error::bad("Use an RFC3339 timestamp with an explicit time zone"))
}
pub fn formatted(value: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(value)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Micros, true))
        .unwrap_or_default()
}
pub fn clock() -> i64 {
    Utc::now().timestamp_micros()
}

pub fn migrate(db: &Connection) -> Result<()> {
    for row in rows(
        db,
        "SELECT repository_id,episode,created_at FROM gf_episodes e WHERE NOT EXISTS(SELECT 1 FROM gf_timeline t WHERE t.repository_id=e.repository_id AND t.episode=e.episode)",
        [],
    )? {
        let at = instant(s(&row, "created_at"))?;
        observe(
            db,
            s(&row, "repository_id"),
            s(&row, "episode"),
            "default",
            at,
            at,
            "recording_time_baseline",
        )?;
    }
    // A pre-migration reverted proposal has lost its acceptance timestamp.
    // Do not invent historical acceptance. Current accepted rows are known.
    for row in rows(
        db,
        "SELECT * FROM gf_alias_proposals p WHERE state='accepted' AND NOT EXISTS(SELECT 1 FROM gf_aliases a WHERE a.repository_id=p.repository_id AND a.proposal_id=p.id)",
        [],
    )? {
        let at = instant(s(&row, "decided_at"))?;
        alias_event(db, s(&row, "repository_id"), &row, true, at, at)?;
    }
    Ok(())
}

pub fn observe(
    db: &Connection,
    repository: &str,
    episode: &str,
    branch: &str,
    valid: i64,
    recorded: i64,
    origin: &str,
) -> Result<()> {
    if branch.is_empty() || branch.len() > 256 || branch.chars().any(char::is_control) {
        return Err(Error::bad("Invalid timeline branch"));
    }
    db.execute("INSERT OR IGNORE INTO gf_timeline(repository_id,branch,episode,valid_us,recorded_us,origin) VALUES(?,?,?,?,?,?)",
        params![repository,branch,episode,valid,recorded,origin])?;
    Ok(())
}

pub struct View {
    pub episode: String,
    pub valid: i64,
    pub known: i64,
    pub metadata: Value,
}

fn optional_timestamp(args: &Value, key: &str) -> Result<Option<i64>> {
    args.get(key)
        .map(|v| {
            v.as_str()
                .ok_or_else(|| Error::bad(format!("{key} must be an RFC3339 timestamp")))
                .and_then(instant)
        })
        .transpose()
}
fn branch(args: &Value) -> Result<&str> {
    let branch = match args.get("branch") {
        None => "default",
        Some(v) => v
            .as_str()
            .ok_or_else(|| Error::bad("branch must be a string"))?,
    };
    if branch.is_empty() || branch.len() > 256 || branch.chars().any(char::is_control) {
        return Err(Error::bad("Invalid timeline branch"));
    }
    Ok(branch)
}

pub fn select(db: &Connection, workspace: &str, repository: &str, args: &Value) -> Result<View> {
    let known = optional_timestamp(args, "known_at")?.unwrap_or_else(clock);
    let branch = branch(args)?;
    if args
        .get("as_of")
        .is_some_and(|v| !v.is_string() && !v.is_null())
    {
        return Err(Error::bad("as_of must be a string"));
    }
    if args.get("canonical").is_some_and(|v| !v.is_boolean()) {
        return Err(Error::bad("canonical must be boolean"));
    }
    let temporal = args.get("valid_at").is_some()
        || args.get("known_at").is_some()
        || args.get("branch").is_some();
    if temporal && args.get("as_of").is_some_and(|v| !v.is_null()) {
        return Err(Error::bad(
            "Choose an episode selector or valid_at/known_at/branch",
        ));
    }
    if temporal {
        let valid = optional_timestamp(args, "valid_at")?.unwrap_or_else(clock);
        let row=one(db,"SELECT episode,valid_us,recorded_us,origin FROM gf_timeline WHERE repository_id=? AND branch=? AND valid_us<=? AND recorded_us<=? ORDER BY valid_us DESC,recorded_us DESC,id DESC LIMIT 1",params![repository,branch,valid,known])?
            .ok_or_else(||Error::bad("No graph known at the requested valid/recorded time"))?;
        Ok(View {
            episode: s(&row, "episode").into(),
            valid,
            known,
            metadata: json!({"valid_at":formatted(valid),"known_at":formatted(known),"branch":branch,"assertion_valid_from":formatted(row["valid_us"].as_i64().unwrap_or(0)),"recorded_at":formatted(row["recorded_us"].as_i64().unwrap_or(0)),"origin":row["origin"]}),
        })
    } else {
        let selector = args["as_of"].as_str().unwrap_or("latest");
        let episode = graph_projection::resolve_episode(db, workspace, repository, selector)?;
        let row = one(
            db,
            "SELECT created_at FROM gf_episodes WHERE repository_id=? AND episode=?",
            params![repository, episode],
        )?
        .ok_or_else(|| Error::bad("Unknown graph episode"))?;
        let valid = if matches!(selector, "latest" | "working") {
            clock()
        } else {
            instant(s(&row, "created_at"))?
        };
        Ok(View {
            episode,
            valid,
            known,
            metadata: json!({"valid_at":formatted(valid),"known_at":formatted(known),"selection":"episode"}),
        })
    }
}

pub fn alias_event(
    db: &Connection,
    repository: &str,
    proposal: &Value,
    active: bool,
    valid: i64,
    recorded: i64,
) -> Result<()> {
    db.execute("INSERT INTO gf_aliases(repository_id,proposal_id,from_node,to_node,active,valid_us,recorded_us) VALUES(?,?,?,?,?,?,?)",
        params![repository,s(proposal,"id"),s(proposal,"from_node"),s(proposal,"to_node"),active,valid,recorded])?;
    Ok(())
}

pub fn aliases_at(db: &Connection, repository: &str, valid: i64, known: i64) -> Result<AliasSet> {
    let active = rows(
        db,
        "SELECT from_node,to_node FROM (SELECT *,row_number() OVER (PARTITION BY proposal_id ORDER BY valid_us DESC,recorded_us DESC,id DESC) AS ordinal FROM gf_aliases WHERE repository_id=? AND valid_us<=? AND recorded_us<=?) WHERE ordinal=1 AND active=1 ORDER BY proposal_id LIMIT 10001",
        params![repository, valid, known],
    )?;
    if active.len() > 10000 {
        return Err(Error::bad("Historical alias view exceeds 10000 aliases"));
    }
    let mut set = AliasSet::new();
    for row in active {
        set.union(s(&row, "from_node"), s(&row, "to_node"));
    }

    Ok(set)
}

pub fn hyperedges_at(
    db: &Connection,
    repository: &str,
    valid: i64,
    known: i64,
    graph: &Graph,
) -> Result<(Vec<Value>, bool)> {
    let mut latest = rows(
        db,
        "SELECT hyperedge_id,episode,relation,members,attributes,active,valid_us,recorded_us FROM (SELECT *,row_number() OVER (PARTITION BY hyperedge_id ORDER BY valid_us DESC,recorded_us DESC,id DESC) AS ordinal FROM gf_hyperedges WHERE repository_id=? AND valid_us<=? AND recorded_us<=?) WHERE ordinal=1 AND active=1 ORDER BY hyperedge_id LIMIT 1001",
        params![repository, valid, known],
    )?;
    let truncated = latest.len() > 1000;
    latest.truncate(1000);
    for row in &mut latest {
        crate::store::decode(row, "members")?;
        crate::store::decode(row, "attributes")?;
    }
    let known_nodes = graph
        .nodes()
        .iter()
        .filter_map(|n| n.id.as_str())
        .collect::<BTreeSet<_>>();
    Ok((
        latest
            .into_iter()
            .map(|mut r| {
                r["unresolved_members"] = json!(
                    r["members"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|v| !v.as_str().is_some_and(|id| known_nodes.contains(id)))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                r["valid_from"] = json!(formatted(r["valid_us"].as_i64().unwrap_or(0)));
                r["recorded_at"] = json!(formatted(r["recorded_us"].as_i64().unwrap_or(0)));
                r
            })
            .collect(),
        truncated,
    ))
}
