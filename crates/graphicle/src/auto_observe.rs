//! Automatic observation: Mastra Observational Memory's observation threshold,
//! opt-in per source connector.
//!
//! Mastra OM observes a thread once its unobserved messages reach
//! `messageTokens` (30,000), off the answering path (mastra-ai/mastra@381c48db,
//! `packages/memory/src/processors/observational-memory/constants.ts` and
//! `observational-memory.ts`, Apache-2.0). A connector's `observe` policy
//! applies the same rule to the connector's sources. Once their unobserved
//! tokens reach `threshold_tokens`, the service queues an `observer_notes`
//! round; a round that stops on its budget queues the next. (A reflection
//! round over the notes, `reflect`, is not supported: unit
//! `observer-reflection`.)
//! Sources stay searchable while unobserved, so the threshold only batches
//! model calls. Each round keeps the policy's call and token limits, at most
//! `max_rounds` rounds run per connector generation (each import or sync),
//! and a failed round stops observation until the next generation.
//! Rounds run with the workspace owner's credential, like scheduled syncs.
//!
//! Mastra later fixed activation leaving the newest, never-buffered messages
//! unobserved above the threshold (mastra-ai/mastra@056427cda): it now keeps
//! activating buffered chunks and then observes the remaining tail. Rounds
//! here buffer nothing: each one observes every unobserved source, and the
//! decision after it re-counts all of them, so a tail that keeps the total at
//! or above the threshold queues the next round at once.
use crate::{
    Error, Result, integer, jobs, memory, model, observer,
    store::{Principal, Store, authorize_hash, decode, repo, rows, s},
    string,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Mastra OM `messageTokens`: unobserved source tokens that start a round.
pub const OBSERVE_TOKENS: usize = 30_000;
/// Rounds per connector generation.
pub const ROUNDS: usize = 8;
const MAX_THRESHOLD: usize = 10_000_000;
const FIELDS: [&str; 8] = [
    "model",
    "profile",
    "max_notes",
    "threshold_tokens",
    "max_rounds",
    "max_calls",
    "max_input_tokens",
    "max_output_tokens",
];

/// Validate a connector `observe` policy. An unsupported field at an inert
/// value (and what only it read) is left out (see [`crate::archived`]).
pub fn validate(policy: &Value) -> Result<()> {
    crate::archived::check(crate::archived::Surface::ObservePolicy, policy)?;
    let policy = &crate::archived::without_inert(crate::archived::Surface::ObservePolicy, policy);
    let fields = policy
        .as_object()
        .ok_or_else(|| Error::bad("observe must be an object"))?;
    if let Some(field) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(Error::bad(format!("Unknown observe policy field {field}")));
    }
    if policy.get("model").is_some() {
        string(policy, "model", 200)?;
    }
    if integer(policy, "threshold_tokens", OBSERVE_TOKENS, MAX_THRESHOLD)? == 0
        || integer(policy, "max_rounds", ROUNDS, 64)? == 0
    {
        return Err(Error::bad(
            "observe thresholds and max_rounds must be positive",
        ));
    }
    integer(policy, "max_calls", 4, 32)?;
    integer(policy, "max_input_tokens", 64000, 512000)?;
    integer(policy, "max_output_tokens", 8192, 64000)?;
    observer::validate_args(&observe_args(policy, "", "connector:policy:"))?;
    Ok(())
}

fn observe_args(policy: &Value, repository: &str, prefix: &str) -> Value {
    let mut args = json!({"kind":observer::KIND,"repository_id":repository,"include":[prefix]});
    for field in [
        "model",
        "profile",
        "max_notes",
        "max_calls",
        "max_input_tokens",
        "max_output_tokens",
    ] {
        if let Some(value) = policy.get(field) {
            args[field] = value.clone();
        }
    }
    args
}

impl Store {
    /// Queue the observation rounds that connector policies call for, for
    /// every connector or only `connector`. Returns one decision per
    /// connector with a policy.
    pub fn auto_observe(
        &self,
        config: &model::Config,
        connector: Option<&str>,
    ) -> Result<Vec<Value>> {
        let connectors = {
            let db = self.connect()?;
            rows(
                &db,
                "SELECT c.id,c.repository_id,c.config,c.generation,p.workspace_id,w.capability_hash FROM am_connectors c JOIN repo_projects p ON p.id=c.repository_id JOIN workspaces w ON w.id=p.workspace_id WHERE c.state='active' AND json_type(c.config,'$.observe')='object' AND (?1 IS NULL OR c.id=?1) ORDER BY c.id",
                [connector],
            )?
        };
        let mut decisions = Vec::new();
        for mut connector in connectors {
            decode(&mut connector, "config")?;
            let decision = crate::settings::configuration(
                self,
                s(&connector, "workspace_id"),
                config,
            )
            .and_then(|config| self.transaction(|db| decide(db, &connector, &config)))
            .unwrap_or_else(
                |e| json!({"connector_id":connector["id"],"queued":false,"reason":e.message}),
            );
            if decision["queued"] == true {
                self.wake.notify_one();
            }
            decisions.push(decision);
        }
        Ok(decisions)
    }
}

/// Whether the latest round stopped before finishing its work.
fn unfinished(rounds: &[&Value]) -> bool {
    rounds
        .last()
        .is_some_and(|r| r["state"] == "complete" && r["result"]["complete"] == false)
}

fn decide(db: &Connection, connector: &Value, config: &model::Config) -> Result<Value> {
    let id = s(connector, "id");
    let repository_id = s(connector, "repository_id");
    let generation = connector["generation"].as_i64().unwrap_or(0);
    let policy = &connector["config"]["observe"];
    let mut decision = json!({"connector_id":id,"generation":generation,"queued":false});
    // A stored policy that turns an unsupported option on stops here, and says why.
    if let Err(error) = crate::archived::check(crate::archived::Surface::ObservePolicy, policy) {
        decision["reason"] = json!(error.message);
        return Ok(decision);
    }
    let Ok(p) = authorize_hash(
        db,
        s(connector, "workspace_id"),
        s(connector, "capability_hash"),
        "propose",
    ) else {
        decision["reason"] = json!("owner credential unavailable");
        return Ok(decision);
    };
    // One automatic round at a time per connector, across generations. A
    // reflection round queued before the archive stays queued (no edition
    // runs it) and is not waited for.
    let active: i64 = db.query_row(
        "SELECT count(*) FROM am_jobs WHERE repository_id=? AND kind=? AND state IN ('queued','retry','running') AND json_extract(payload,'$.auto.connector_id')=?",
        params![repository_id, observer::KIND, id],
        |r| r.get(0),
    )?;
    if active > 0 {
        decision["reason"] = json!("round in progress");
        return Ok(decision);
    }
    let mut rounds = rows(
        db,
        "SELECT kind,state,result FROM am_jobs WHERE repository_id=? AND kind=? AND json_extract(payload,'$.auto.connector_id')=? AND json_extract(payload,'$.auto.generation')=? ORDER BY created_at,rowid",
        params![repository_id, observer::KIND, id, generation],
    )?;
    for round in &mut rounds {
        decode(round, "result")?;
    }
    if rounds
        .iter()
        .any(|r| ["failed", "cancelled"].contains(&s(r, "state")))
    {
        decision["reason"] = json!("a round failed; waiting for the next import or sync");
        return Ok(decision);
    }
    let max_rounds = integer(policy, "max_rounds", ROUNDS, 64)?;
    let repository = repo(db, &p, repository_id)?;
    let documents = memory::documents(db, &repository, &p)?;
    let prefix = format!("connector:{id}:");
    let args = observe_args(policy, repository_id, &prefix);
    let (_, profile) = config.profile(args["model"].as_str())?;
    let recipe = observer::Recipe::new(&args, profile)?;
    let observed: BTreeMap<String, String> = rows(
        db,
        "SELECT source_id,step_key FROM am_observer_notes WHERE repository_id=?",
        [repository_id],
    )?
    .iter()
    .map(|r| (s(r, "source_id").to_string(), s(r, "step_key").to_string()))
    .collect();
    let unobserved: usize = documents
        .iter()
        .filter(|d| s(d, "id").starts_with(&prefix))
        .filter(|d| {
            observed
                .get(s(d, "id"))
                .is_none_or(|key| *key != recipe.key(d))
        })
        .map(|d| graphicle_memory::text::count(s(d, "content")))
        .sum();
    decision["unobserved_tokens"] = json!(unobserved);
    let observing: Vec<&Value> = rounds
        .iter()
        .filter(|r| r["kind"] == observer::KIND)
        .collect();
    let threshold = integer(policy, "threshold_tokens", OBSERVE_TOKENS, MAX_THRESHOLD)?;
    let mut idle = if unobserved > 0 {
        "below threshold"
    } else {
        "observed"
    };
    if unobserved > 0 && (unobserved >= threshold || unfinished(&observing)) {
        if observing.len() < max_rounds {
            return enqueue(db, &p, args, config, decision, generation);
        }
        idle = "observation rounds exhausted for this generation";
    }
    decision["reason"] = json!(idle);
    Ok(decision)
}

fn enqueue(
    db: &Connection,
    p: &Principal,
    mut args: Value,
    config: &model::Config,
    mut decision: Value,
    generation: i64,
) -> Result<Value> {
    jobs::check(&args, config)?;
    args["auto"] = json!({"connector_id":decision["connector_id"],"generation":generation});
    let job = jobs::queue(db, p, &args, config)?;
    decision["kind"] = args["kind"].clone();
    decision["job_id"] = job["id"].clone();
    if job["replayed"] == true {
        // Nothing changed since an identical round, which already ran.
        decision["reason"] = json!("unchanged since the last round");
    } else {
        decision["queued"] = json!(true);
    }
    Ok(decision)
}
