//! Retention of per-episode graph by-products and the `graph.stats` size
//! report.
//!
//! Every projected episode leaves behind rows that are only useful for a
//! while: execution coverage (`gf_test_coverage`, `gf_coverage_uploads`,
//! `gf_coverage_inventory`),
//! closed cross-repository edges (`gf_cross_edges`, `gf_cross_unresolved`)
//! and receiver-resolution leftovers (`gf_unresolved_calls`). [`Retention`]
//! bounds each of them to the last *N* episodes of a repository (by
//! `gf_episodes.sequence`); nodes, edges and episodes themselves are never
//! removed here. Defaults: coverage 20 episodes, cross edges 20 episodes,
//! unresolved calls 2 (current + previous). Override per call with
//! `retention` arguments or process-wide with `GRAPHICLE_RETENTION_COVERAGE`,
//! `GRAPHICLE_RETENTION_CROSS_EDGES`, `GRAPHICLE_RETENTION_UNRESOLVED`.
use crate::{
    Error, Result,
    store::{Store, one, repo, rows, s},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_COVERAGE_EPISODES: usize = 20;
pub const DEFAULT_CROSS_EDGE_EPISODES: usize = 20;
pub const DEFAULT_UNRESOLVED_EPISODES: usize = 2;
const MAX_EPISODES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Episodes (newest first) whose coverage rows and upload records stay.
    pub coverage_episodes: usize,
    /// Episodes whose *closed* cross-repository edge rows stay; open rows
    /// (`valid_to_episode IS NULL`) always stay.
    pub cross_edge_episodes: usize,
    /// Episodes whose `unresolved_calls` rows stay (2 = current + previous).
    pub unresolved_episodes: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            coverage_episodes: DEFAULT_COVERAGE_EPISODES,
            cross_edge_episodes: DEFAULT_CROSS_EDGE_EPISODES,
            unresolved_episodes: DEFAULT_UNRESOLVED_EPISODES,
        }
    }
}

impl Retention {
    /// Defaults overridden by the `GRAPHICLE_RETENTION_*` environment.
    pub fn from_env() -> Self {
        let read = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|v| (1..=MAX_EPISODES).contains(v))
                .unwrap_or(default)
        };
        Self {
            coverage_episodes: read("GRAPHICLE_RETENTION_COVERAGE", DEFAULT_COVERAGE_EPISODES),
            cross_edge_episodes: read(
                "GRAPHICLE_RETENTION_CROSS_EDGES",
                DEFAULT_CROSS_EDGE_EPISODES,
            ),
            unresolved_episodes: read(
                "GRAPHICLE_RETENTION_UNRESOLVED",
                DEFAULT_UNRESOLVED_EPISODES,
            ),
        }
    }

    /// Environment defaults overridden by an optional `retention` object in
    /// the request (`coverage_episodes`, `cross_edge_episodes`,
    /// `unresolved_episodes`; each 1..=100000).
    pub fn from_args(args: &Value) -> Result<Self> {
        let mut retention = Self::from_env();
        let Some(overrides) = args.get("retention") else {
            return Ok(retention);
        };
        if !overrides.is_object() {
            return Err(Error::bad("Expected a retention object"));
        }
        for (key, slot) in [
            ("coverage_episodes", &mut retention.coverage_episodes),
            ("cross_edge_episodes", &mut retention.cross_edge_episodes),
            ("unresolved_episodes", &mut retention.unresolved_episodes),
        ] {
            match overrides.get(key) {
                None | Some(Value::Null) => {}
                Some(value) => {
                    *slot = value
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .filter(|v| (1..=MAX_EPISODES).contains(v))
                        .ok_or_else(|| {
                            Error::bad(format!("Expected retention.{key} between 1 and 100000"))
                        })?;
                }
            }
        }
        Ok(retention)
    }
}

/// Episodes that fall outside the newest `keep` for `repository`, as an
/// SQL fragment usable in `episode IN (...)`.
const OLDER_THAN: &str = "SELECT episode FROM gf_episodes WHERE repository_id=?1 \
     AND sequence <= (SELECT coalesce(max(sequence),0) FROM gf_episodes WHERE repository_id=?1) - ?2";

/// Apply `retention` to one repository. Returns per-table deleted row counts.
pub fn apply(db: &Connection, repository: &str, retention: &Retention) -> Result<Value> {
    let coverage = db.execute(
        &format!(
            "DELETE FROM gf_test_coverage WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    let uploads = db.execute(
        &format!(
            "DELETE FROM gf_coverage_uploads WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    let inventories = db.execute(
        &format!(
            "DELETE FROM gf_coverage_inventory WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    // Closed cross edges are history: drop those whose *own* side of the
    // edge points at an episode this repository no longer keeps. Open edges
    // describe the current link state and always stay.
    let cross_edges = db.execute(
        &format!(
            "DELETE FROM gf_cross_edges WHERE valid_to_episode IS NOT NULL AND (\
               (source_repository_id=?1 AND source_episode IN ({OLDER_THAN})) OR \
               (target_repository_id=?1 AND target_episode IN ({OLDER_THAN})))"
        ),
        params![repository, retention.cross_edge_episodes as i64],
    )?;
    let cross_unresolved = db.execute(
        &format!(
            "DELETE FROM gf_cross_unresolved WHERE valid_to_episode IS NOT NULL \
             AND source_repository_id=?1 AND source_episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.cross_edge_episodes as i64],
    )?;
    let unresolved = db.execute(
        &format!(
            "DELETE FROM gf_unresolved_calls WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.unresolved_episodes as i64],
    )?;
    Ok(json!({
        "retention": retention,
        "deleted": {
            "coverage": coverage,
            "coverage_uploads": uploads,
            "coverage_inventory": inventories,
            "cross_edges": cross_edges,
            "cross_unresolved": cross_unresolved,
            "unresolved_calls": unresolved,
        }
    }))
}

/// Rows [`apply`] would delete now, per table (all zero right after it ran).
pub fn remaining(db: &Connection, repository: &str, retention: &Retention) -> Result<Value> {
    let count = |sql: &str, keep: usize| -> Result<i64> {
        Ok(db.query_row(sql, params![repository, keep as i64], |r| r.get(0))?)
    };
    Ok(json!({
        "coverage": count(&format!("SELECT count(*) FROM gf_test_coverage WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "coverage_uploads": count(&format!("SELECT count(*) FROM gf_coverage_uploads WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "coverage_inventory": count(&format!("SELECT count(*) FROM gf_coverage_inventory WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "cross_edges": count(&format!(
            "SELECT count(*) FROM gf_cross_edges WHERE valid_to_episode IS NOT NULL AND (\
               (source_repository_id=?1 AND source_episode IN ({OLDER_THAN})) OR \
               (target_repository_id=?1 AND target_episode IN ({OLDER_THAN})))"), retention.cross_edge_episodes)?,
        "cross_unresolved": count(&format!(
            "SELECT count(*) FROM gf_cross_unresolved WHERE valid_to_episode IS NOT NULL \
             AND source_repository_id=?1 AND source_episode IN ({OLDER_THAN})"), retention.cross_edge_episodes)?,
        "unresolved_calls": count(&format!("SELECT count(*) FROM gf_unresolved_calls WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.unresolved_episodes)?,
    }))
}

// ------------------------------------------------------------ source retention

/// Which timestamp dates a connector document for [`SourcePolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dated {
    /// The provider's own timestamp (`source_timestamp`, else the provider
    /// metadata's `updated_at` / `created_at`).
    SourceTimestamp,
    /// When the connector last observed the document.
    ObservedAt,
}

/// Time-based retention of connector documents: documents older than
/// `max_age_days` are forgotten through certified deletion, one receipt per
/// batch. Nothing runs on a schedule; `retention.preview` shows the plan and
/// `retention.apply` must echo its `plan_hash`. Documents with no usable
/// date are never selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePolicy {
    pub connector_ids: Vec<String>,
    pub max_age_days: u32,
    pub by: Dated,
    /// Keep documents a reviewed memory cites (default true).
    pub skip_cited_by_reviewed: bool,
}

impl SourcePolicy {
    /// `sources: {connector_ids, max_age_days, by?, skip_cited_by_reviewed?}`.
    pub fn from_args(args: &Value) -> Result<Self> {
        let sources = args
            .get("sources")
            .filter(|v| v.is_object())
            .ok_or_else(|| Error::bad("Expected a sources object"))?;
        let mut connector_ids: Vec<String> = sources["connector_ids"]
            .as_array()
            .filter(|ids| !ids.is_empty() && ids.len() <= 100)
            .ok_or_else(|| Error::bad("sources.connector_ids must list 1–100 connectors"))?
            .iter()
            .map(|id| {
                id.as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 100)
                    .map(str::to_string)
                    .ok_or_else(|| Error::bad("Invalid connector id"))
            })
            .collect::<Result<_>>()?;
        connector_ids.sort();
        connector_ids.dedup();
        let max_age_days = sources["max_age_days"]
            .as_u64()
            .filter(|d| (1..=36_500).contains(d))
            .ok_or_else(|| Error::bad("sources.max_age_days must be between 1 and 36500"))?
            as u32;
        let by = match sources.get("by").and_then(Value::as_str) {
            None | Some("source_timestamp") => Dated::SourceTimestamp,
            Some("observed_at") => Dated::ObservedAt,
            Some(_) => {
                return Err(Error::bad(
                    "sources.by must be source_timestamp or observed_at",
                ));
            }
        };
        let skip_cited_by_reviewed = match sources.get("skip_cited_by_reviewed") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(on)) => *on,
            Some(_) => {
                return Err(Error::bad(
                    "sources.skip_cited_by_reviewed must be true or false",
                ));
            }
        };
        Ok(Self {
            connector_ids,
            max_age_days,
            by,
            skip_cited_by_reviewed,
        })
    }
}

/// What a [`SourcePolicy`] selects now.
#[derive(Clone, Debug, Default)]
pub struct SourcePlan {
    pub cutoff: String,
    /// Connector → selected document ids.
    pub documents: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub skipped_cited: usize,
    pub skipped_undated: usize,
    /// Digest of the policy and the selected documents' one-way names.
    pub plan_hash: String,
}

fn document_date(metadata: &Value, by: Dated) -> Option<chrono::DateTime<chrono::Utc>> {
    let candidates: Vec<&Value> = match by {
        Dated::ObservedAt => vec![&metadata["observed_at"]],
        Dated::SourceTimestamp => vec![
            &metadata["source_timestamp"],
            &metadata["provider_metadata"]["updated_at"],
            &metadata["provider_metadata"]["created_at"],
        ],
    };
    candidates.into_iter().find_map(|value| {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    })
}

/// Select the documents `policy` would forget at `now`.
pub fn source_plan(
    db: &Connection,
    principal: &crate::store::Principal,
    repository: &Value,
    policy: &SourcePolicy,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SourcePlan> {
    let identifier = s(repository, "id");
    let cutoff = now - chrono::Duration::days(i64::from(policy.max_age_days));
    let mut cited = std::collections::BTreeSet::new();
    if policy.skip_cited_by_reviewed {
        for memory in crate::memory::list(db, repository, principal, true)? {
            if memory["state"] != "reviewed" {
                continue;
            }
            for field in ["native_sources", "evidence_links"] {
                if let Some(sources) = memory["data"][field].as_object() {
                    cited.extend(sources.keys().cloned());
                }
            }
        }
    }
    let mut plan = SourcePlan {
        cutoff: cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ..SourcePlan::default()
    };
    let mut hashes: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for connector in &policy.connector_ids {
        one(
            db,
            "SELECT id FROM am_connectors WHERE repository_id=? AND id=?",
            params![identifier, connector],
        )?
        .ok_or_else(|| Error::bad(format!("Connector unavailable: {connector}")))?;
        for doc in rows(
            db,
            "SELECT id,metadata FROM am_connector_docs WHERE connector_id=? ORDER BY id",
            [connector],
        )? {
            let metadata: Value = serde_json::from_str(s(&doc, "metadata")).unwrap_or(Value::Null);
            let Some(date) = document_date(&metadata, policy.by) else {
                plan.skipped_undated += 1;
                continue;
            };
            if date >= cutoff {
                continue;
            }
            let id = s(&doc, "id").to_string();
            if cited.contains(&format!("connector:{connector}:{id}")) {
                plan.skipped_cited += 1;
                continue;
            }
            let stamp = date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            if plan.oldest.as_ref().is_none_or(|o| stamp < *o) {
                plan.oldest = Some(stamp.clone());
            }
            if plan.newest.as_ref().is_none_or(|n| stamp > *n) {
                plan.newest = Some(stamp);
            }
            hashes
                .entry(connector.as_str())
                .or_default()
                .push(crate::deletion::document_hash(connector, &id));
            plan.documents
                .entry(connector.clone())
                .or_default()
                .insert(id);
        }
    }
    for list in hashes.values_mut() {
        list.sort();
    }
    plan.plan_hash = crate::digest(&json!({"policy": policy, "documents": hashes}));
    Ok(plan)
}

impl Store {
    /// `retention.preview` (read): what a [`SourcePolicy`] would forget now,
    /// per connector with the rows certified deletion would remove, and the
    /// `plan_hash` `retention.apply` must echo. Deletes nothing.
    pub fn retention_preview(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let policy = SourcePolicy::from_args(args)?;
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let identifier = s(&repository, "id");
            let plan = source_plan(db, p, &repository, &policy, chrono::Utc::now())?;
            let mut connectors = serde_json::Map::new();
            for (connector, documents) in &plan.documents {
                let mut removes: std::collections::BTreeMap<&str, usize> = Default::default();
                let mut derived: std::collections::BTreeMap<&str, usize> = Default::default();
                let listed: Vec<&String> = documents.iter().collect();
                for batch in listed.chunks(crate::deletion::MAX_SCOPE_DOCUMENTS) {
                    let scope = crate::deletion::Scope::document_set(
                        connector,
                        batch.iter().map(|d| (*d).clone()).collect(),
                    );
                    let forget = crate::deletion::plan(db, &p.workspace, identifier, &scope)?;
                    for (table, n) in forget.rows() {
                        *removes.entry(table).or_default() += n;
                    }
                    for (kind, n) in forget.visible_since() {
                        *derived.entry(kind).or_default() += n;
                    }
                }
                connectors.insert(
                    connector.clone(),
                    json!({"documents": documents.len(), "removes": removes, "derived_since_visible": derived}),
                );
            }
            Ok(json!({
                "policy": policy,
                "cutoff": plan.cutoff,
                "documents": plan.documents.values().map(|d| d.len()).sum::<usize>(),
                "date_range": {"oldest": plan.oldest, "newest": plan.newest},
                "connectors": connectors,
                "skipped": {"cited_by_reviewed": plan.skipped_cited, "undated": plan.skipped_undated},
                "plan_hash": plan.plan_hash,
                "apply": "retention.apply with the same sources and this plan_hash; each batch of up to 1,000 documents is forgotten with its own receipt",
                "scheduled": false,
            }))
        })
    }
}

/// Tables reported by [`stats`]: name → (SQL filter, text columns whose
/// lengths approximate the row's content bytes).
const TABLES: &[(&str, &str, &str, &[&str])] = &[
    (
        "nodes",
        "gf_nodes",
        "repository_id=?1",
        &[
            "node_id",
            "attributes",
            "valid_from_episode",
            "valid_to_episode",
        ],
    ),
    (
        "edges",
        "gf_edges",
        "repository_id=?1",
        &[
            "source",
            "target",
            "relation",
            "key",
            "attributes",
            "valid_from_episode",
            "valid_to_episode",
        ],
    ),
    (
        "edge_sources",
        "gf_edge_sources",
        "repository_id=?1",
        &["relpath", "source_location", "derivation"],
    ),
    (
        "coverage",
        "gf_test_coverage",
        "repository_id=?1",
        &["episode", "test_id", "test_node_id", "relpath"],
    ),
    (
        "coverage_inventory",
        "gf_coverage_inventory",
        "repository_id=?1",
        &["episode", "inventory"],
    ),
    (
        "cross_edges",
        "gf_cross_edges",
        "source_repository_id=?1 OR target_repository_id=?1",
        &[
            "source_episode",
            "source_node_id",
            "source_stable_id",
            "target_episode",
            "target_node_id",
            "target_stable_id",
            "relation",
            "module_path",
            "symbol",
        ],
    ),
    (
        "unresolved_calls",
        "gf_unresolved_calls",
        "repository_id=?1",
        &["episode", "caller", "callee", "call"],
    ),
];

/// Per-repository size report: row counts and *content bytes* per table
/// (the sum of the text columns' lengths plus 8 bytes per integer column —
/// an approximation of what the rows carry, not SQLite page usage, which
/// includes indexes and free space and is only available per database),
/// episode count, and the latest `impact.report` wall-clock time.
pub fn stats(db: &Connection, repository: &str) -> Result<Value> {
    let mut tables = serde_json::Map::new();
    let mut total_bytes = 0i64;
    for (name, table, filter, columns) in TABLES {
        let lengths = columns
            .iter()
            .map(|c| format!("coalesce(length({c}),0)"))
            .collect::<Vec<_>>()
            .join("+");
        let row = one(
            db,
            &format!("SELECT count(*) AS rows_, coalesce(sum({lengths}),0)+count(*)*16 AS bytes FROM {table} WHERE {filter}"),
            [repository],
        )?
        .unwrap_or_else(|| json!({"rows_": 0, "bytes": 0}));
        let bytes = row["bytes"].as_i64().unwrap_or(0);
        total_bytes += bytes;
        tables.insert(
            name.to_string(),
            json!({"rows": row["rows_"].as_i64().unwrap_or(0), "content_bytes": bytes}),
        );
    }
    let episodes = one(
        db,
        "SELECT count(*) AS n, coalesce(sum(length(attributes)),0) AS attribute_bytes FROM gf_episodes WHERE repository_id=?",
        [repository],
    )?
    .unwrap_or_else(|| json!({"n": 0, "attribute_bytes": 0}));
    let uploads = rows(
        db,
        "SELECT episode, source, granularity, tests, files, ranges, created_at FROM gf_coverage_uploads WHERE repository_id=? ORDER BY id DESC LIMIT 20",
        [repository],
    )?;
    let last_report = one(
        db,
        "SELECT elapsed_ms, changed_files, base_episode, head_episode, created_at FROM gf_report_timings WHERE repository_id=? ORDER BY id DESC LIMIT 1",
        [repository],
    )?;
    let report_count: i64 = db.query_row(
        "SELECT count(*) FROM gf_report_timings WHERE repository_id=?",
        [repository],
        |r| r.get(0),
    )?;
    let database_bytes: i64 = db.query_row(
        "SELECT page_count*page_size FROM pragma_page_count(), pragma_page_size()",
        [],
        |r| r.get(0),
    )?;
    Ok(json!({
        "repository_id": repository,
        "episodes": episodes["n"],
        "episode_attribute_bytes": episodes["attribute_bytes"],
        "tables": tables,
        "content_bytes": total_bytes,
        "database_bytes_all_repositories": database_bytes,
        "coverage_uploads": uploads,
        "impact_reports": report_count,
        "last_impact_report": last_report.map(|r| json!({
            "elapsed_ms": r["elapsed_ms"],
            "changed_files": r["changed_files"],
            "base_episode": r["base_episode"],
            "head_episode": r["head_episode"],
            "created_at": r["created_at"],
        })),
        "retention_defaults": Retention::from_env(),
        "bytes_note": "content_bytes sums the text columns of the rows (+16 bytes/row); database_bytes_all_repositories is the SQLite file size, shared by every repository.",
    }))
}

impl Store {
    /// `graph.stats` (read): size and timing summary for one repository.
    pub fn graph_stats(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            stats(db, s(&repository, "id"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let base = Retention::default();
        assert_eq!(base.coverage_episodes, 20);
        assert_eq!(base.cross_edge_episodes, 20);
        assert_eq!(base.unresolved_episodes, 2);
        let over = Retention::from_args(&json!({"retention": {"coverage_episodes": 3}})).unwrap();
        assert_eq!(over.coverage_episodes, 3);
        assert_eq!(over.unresolved_episodes, base.unresolved_episodes);
        assert!(Retention::from_args(&json!({"retention": {"coverage_episodes": 0}})).is_err());
        assert!(Retention::from_args(&json!({"retention": 5})).is_err());
    }
}
