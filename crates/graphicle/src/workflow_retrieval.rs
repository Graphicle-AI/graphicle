//! Retrieval options and local scoring shared by durable answering workflows.
//!
//! A workflow owns its evidence ceiling and source scope. Caller-selected
//! ranking and packing settings survive each initial and follow-up search.
//! Local scorers use the same implementation as interactive context, with
//! pinned profiles, job reservations and actual CPU usage in the ledger.
use crate::{
    Error, Result, digest, id, integer,
    jobs::{self, Job, Worker},
    model, now, query,
    store::one,
};
use rusqlite::params;
use serde_json::{Value, json};

const PROFILES: [&str; 1] = ["local_reranker_profile"];
const KEYS: &str = "local_retrieval_keys";
const OPTIONS: &[&str] = &[
    "stem",
    "speaker",
    "pack_focus",
    "origin_quota",
    "arm_weights",
    "rrf_k",
    "query_variants",
    "note_keys",
    "rerank_depth",
    "rerank_mode",
    "compact_citations",
    "pack_order",
    "shown",
    "reading_guidance",
    "expand_context",
    "memory_retrieval",
    "graph_retrieval",
    "doc_links",
    "symbol_links",
    "notes_first",
    "code_first",
    "query_focus",
    "more_files",
    "mode",
    "needs",
];

pub(crate) fn arguments(payload: &Value, mut bounded: Value) -> Value {
    for &key in OPTIONS {
        if bounded.get(key).is_none()
            && let Some(value) = payload.get(key)
        {
            bounded[key] = value.clone();
        }
    }
    bounded
}

pub(crate) fn scorer_count(payload: &Value) -> usize {
    PROFILES
        .iter()
        .filter(|field| payload[**field].is_string())
        .count()
}

pub(crate) fn diagnostics(packet: &Value) -> Value {
    json!({"question":packet["question"],"retrieval_defaults":packet["retrieval_defaults"],
        "arm_weights":packet["arm_weights"],"local_reranker":packet["local_reranker"],
        "mechanisms":packet["mechanisms"],"code_graph":packet["code_graph"],
        "depth":packet["depth"],"source_windows":packet["evidence"].as_array().map_or(0,Vec::len)})
}

pub(crate) fn check(kind: &str, args: &Value, config: &model::Config) -> Result<()> {
    if args.get(KEYS).is_some() {
        return Err(Error::bad("local_retrieval_keys are pinned by the service"));
    }
    for field in PROFILES.into_iter().chain(["local_embedding_profile"]) {
        if args.get(field).is_none_or(Value::is_null) {
            continue;
        }
        if !["basic", "reflect", "recall"].contains(&kind) {
            return Err(Error::bad(format!(
                "{field} applies only to retrieval workflows"
            )));
        }
        let name = crate::string(args, field, 100)?;
        let (_, profile) = config.profile(Some(name))?;
        if profile.protocol != "local_onnx" {
            return Err(Error::bad(format!("{field} requires a local_onnx profile")));
        }
        crate::local_inference::validate(profile)?;
        if field == "local_embedding_profile" {
            let other = if kind == "recall" {
                "model"
            } else {
                "embedding_model"
            };
            if args["semantic_retrieval"] == false
                || args[other]
                    .as_str()
                    .is_some_and(|selected| selected != name)
            {
                return Err(Error::bad("Conflicting embedding retrieval settings"));
            }
        }
    }
    Ok(())
}

pub(crate) fn pin(payload: &mut Value, config: &model::Config) -> Result<()> {
    let mut keys = json!({});
    for field in PROFILES {
        if let Some(name) = payload[field].as_str() {
            keys[field] = json!(model::key(config.profile(Some(name))?.1));
        }
    }
    if keys.as_object().is_some_and(|keys| !keys.is_empty()) {
        payload[KEYS] = keys;
    }
    Ok(())
}

/// A conservative token reservation from the pinned scorer's sequence cap.
/// The actual attended-token count replaces it on successful completion.
fn token_bound(profile: &model::Profile, candidates: usize) -> Result<usize> {
    let path = crate::local_inference::directory(profile)?.join("MODEL.json");
    if std::fs::metadata(&path)?.len() > 65536 {
        return Err(Error::bad("Model manifest exceeds 64 KiB"));
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let length = manifest["max_length"]
        .as_u64()
        .filter(|n| (8..=1024).contains(n))
        .ok_or_else(|| Error::bad("Unsupported local scoring sequence length"))?
        as usize;
    if manifest["model"] != profile.model || manifest["pooling"] != "logits" {
        return Err(Error::bad(
            "Local retrieval profile has the wrong scoring role",
        ));
    }
    Ok(candidates * length)
}

/// `reserve` keeps controller/answer calls available after this retrieval.
pub(crate) fn context(
    worker: &Worker,
    job: &Job,
    snapshot: &query::Snapshot,
    bounded: Value,
    rankings: Value,
    reserve: usize,
) -> Result<Value> {
    let args = arguments(&job.payload, bounded);
    let links = crate::doc_links::Options::parse(&args)?;
    let graph = if args["graph_retrieval"] == false {
        crate::code_graph_arm::Arm::skipped("disabled")
    } else {
        jobs::guarded_read(&worker.store, job, |db, _, _| {
            let question = crate::string(&args, "question", 8000)?;
            // The arm reads a code question's content, and leaves document
            // nodes out for it as for `doc_links: false`, as `context` does.
            let focus = crate::code_question::focus(&args, question)?;
            let code_first = crate::code_question::decide(&args, question, &focus.text)?;
            Ok(crate::code_graph_arm::arm_with(
                db,
                &job.repository,
                snapshot,
                &focus.text,
                links.documents() && !code_first.applied,
            )
            .unwrap_or_else(|error| {
                let mut arm = crate::code_graph_arm::Arm::skipped("error");
                arm.diagnostics["error"] = json!(error.message);
                arm
            }))
        })?
    };
    // The documents' mention links and reviewed memory's links to code
    // symbols, read on a connection of their own: rows only, checked against
    // the snapshot (each followed with its switch `true`, else counted). The
    // memory links are read with `doc_links: false` too; `context_on` reads
    // the document links only when documents take part.
    let symbol_links = crate::symbol_links::Mode::parse(&args)?;
    let links_db = if links.documents() || symbol_links != crate::symbol_links::Mode::Off {
        let db = worker.store.connect()?;
        db.pragma_update(None, "query_only", true)?;
        Some(db)
    } else {
        None
    };
    let lookup = links_db
        .as_ref()
        .map(|db| crate::doc_links::Lookup::new(db, &job.repository));
    if scorer_count(&job.payload) == 0 {
        return query::context_on(
            snapshot,
            &args,
            rankings,
            Some(&graph),
            None,
            None,
            lookup.as_ref(),
        );
    }
    let config = crate::settings::configuration(&worker.store, &job.workspace, &worker.config)?;
    let mut selected = Vec::new();
    let limit = integer(&args, "limit", 8, 40)?.max(1);
    let depth = integer(&args, "rerank_depth", 32, query::MAX_RERANK_DEPTH)?.max(limit.min(32));
    // Reserve the requested prefix without rebuilding every source passage
    // just to count it. Linked evidence can add candidates after that count.
    let candidates = if snapshot.documents.is_empty() && snapshot.memories.is_empty() {
        0
    } else {
        depth
    };
    for field in PROFILES {
        if let Some(name) = job.payload[field].as_str() {
            let (_, profile) = config.profile(Some(name))?;
            let key = model::key(profile);
            if job.payload[KEYS][field] != key {
                return Err(Error::conflict(
                    "Local retrieval configuration changed after queuing",
                ));
            }
            let tokens = token_bound(profile, candidates)?;
            selected.push((field, profile, key, tokens, id()));
        }
    }
    let active = usize::from(candidates > 0) * selected.len();
    let reserved: usize = selected.iter().map(|(_, _, _, tokens, _)| *tokens).sum();
    if active > 0 {
        jobs::guarded(&worker.store, job, |db, _, _| {
            jobs::renew(db, job)?;
            let totals = one(db, "SELECT count(*) AS calls,coalesce(sum(input_reserved),0) AS input FROM am_call_reservations WHERE job_id=?", [&job.id])?.unwrap();
            if totals["calls"].as_u64().unwrap_or(0) + (active + reserve) as u64
                > job.payload["max_calls"].as_u64().unwrap_or(4)
                || totals["input"].as_u64().unwrap_or(0) + reserved as u64
                    > job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
            {
                return Err(Error::bad(
                    "Local retrieval budget exhausted; keep room for the answer",
                ));
            }
            for (field, profile, key, tokens, call) in &selected {
                let hash = digest(
                    &json!({"kind":field,"profile":key,"source_state":snapshot.state,
                    "principal":job.principal_hash,"repository":job.repository,"arguments":args,"rankings":rankings}),
                );
                db.execute("INSERT INTO am_call_reservations(id,job_id,request_hash,state,input_reserved,output_reserved,created_at) VALUES(?,?,?,'started',?,0,?)",
                    params![call,job.id,hash,tokens,now()])?;
                db.execute(
                    "INSERT INTO am_model_calls VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        call,
                        job.repository,
                        job.id,
                        profile.protocol,
                        profile.model,
                        hash,
                        0,
                        0,
                        0,
                        0,
                        now()
                    ],
                )?;
            }
            Ok(())
        })?;
    }
    let profile = |field: &str| {
        selected
            .iter()
            .find(|(key, _, _, _, _)| *key == field)
            .map(|(_, p, _, _, _)| *p)
    };
    let started = std::time::Instant::now();
    let result = query::context_on(
        snapshot,
        &args,
        rankings,
        Some(&graph),
        profile("local_reranker_profile"),
        None,
        lookup.as_ref(),
    );
    if active > 0 {
        worker.store.transaction(|db| {
            for (_, _, _, _, call) in &selected {
                match &result {
                    Ok(packet) => {
                        let usage = &packet["local_reranker"];
                        let tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                        let elapsed = usage["elapsed_ms"].as_f64().unwrap_or(0.0);
                        db.execute("UPDATE am_call_reservations SET state='finished',input_reserved=?,usage_reported=1,finished_at=? WHERE id=?",
                            params![tokens,now(),call])?;
                        db.execute("UPDATE am_model_calls SET input_tokens=?,elapsed_ms=? WHERE id=?",params![tokens,elapsed,call])?;
                    }
                    Err(error) => {
                        db.execute("UPDATE am_call_reservations SET state='unknown_usage',error=?,finished_at=? WHERE id=?",params![error.message,now(),call])?;
                        db.execute("UPDATE am_model_calls SET elapsed_ms=? WHERE id=?",params![started.elapsed().as_millis() as u64,call])?;
                    }
                }
            }
            Ok(())
        })?;
    }
    let mut packet = result?;
    for (_, profile, key, _, _) in &selected {
        if model::key(profile) != *key {
            return Err(Error::conflict(
                "Local retrieval manifest changed during scoring",
            ));
        }
    }
    jobs::guarded_read(&worker.store, job, |_, _, _| Ok(()))?;
    packet["local_retrieval_job_calls"] = json!(active);
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_limits_override_payload_but_false_and_follow_up_options_survive() {
        let args = arguments(
            &json!({"question":"old","max_tokens":64000,"graph_retrieval":true,
            "stem":false,"pack_focus":false,"query_variants":[{"text":"old"}],"pack_order":"chronological"}),
            json!({"question":"new","max_tokens":700,"graph_retrieval":false,"query_variants":[]}),
        );
        assert_eq!(args["question"], "new");
        assert_eq!(args["max_tokens"], 700);
        assert_eq!(args["graph_retrieval"], false);
        assert_eq!(args["stem"], false);
        assert_eq!(args["pack_focus"], false);
        assert_eq!(args["query_variants"], json!([]));
        assert_eq!(args["pack_order"], "chronological");
    }
}
