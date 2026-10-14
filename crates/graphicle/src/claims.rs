//! Grounded interpretation drafts and explicitly reviewed temporal revisions.
use crate::{Error, Result, digest, id, native, now, string};
use crate::{
    memory,
    store::{Principal, Store, decode, one, repo, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn current(db: &Connection, repository: &Value, p: &Principal) -> Result<Vec<Value>> {
    let memories = memory::list(db, repository, p, true)?;
    let allowed: BTreeMap<&str, &Value> = memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] != true)
        .map(|m| (s(m, "id"), m))
        .collect();
    let mut output = Vec::new();
    for mut row in rows(
        db,
        "SELECT * FROM am_claims WHERE repository_id=? AND state='active' ORDER BY id",
        [s(repository, "id")],
    )? {
        if let Some(memory) = allowed.get(s(&row, "memory_id")) {
            decode(&mut row, "data")?;
            row["memory_revision"] = memory["revision"].clone();
            output.push(row);
        }
    }
    Ok(output)
}
pub fn propose(
    db: &Connection,
    repository: &Value,
    p: &Principal,
    claims: &Value,
    sources: &[Value],
    targets: &[Value],
    provider: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let checked = native(
        "interpretation.validate",
        json!({"scope":identifier,"claims":claims,"sources":sources,"target_ids":targets.iter().map(|t|t["id"].clone()).collect::<Vec<_>>()}),
    )?;
    let docs = memory::documents(db, repository, p)?;
    // Stamp the view of the sources the claims were read from, not of every
    // document: a claim then carries no trace of unrelated sources, which
    // forgetting one of them requires.
    let read: Vec<Value> = docs
        .iter()
        .filter(|d| sources.iter().any(|x| x["id"] == d["id"]))
        .cloned()
        .collect();
    let source_state = memory::state(&read);
    let mut output = Vec::new();
    for claim in checked["claims"].as_array().unwrap() {
        let claim_id = s(claim, "id");
        if let Some(prior) = one(
            db,
            "SELECT memory_id,state FROM am_claims WHERE repository_id=? AND id=?",
            params![identifier, claim_id],
        )? {
            output.push(json!({"claim_id":claim_id,"memory_id":prior["memory_id"],"state":prior["state"],"replayed":true}));
            continue;
        }
        let mut data =
            memory::dependencies(db, identifier, claim["evidence"].as_array().unwrap(), &docs)?;
        for source in sources {
            if let Some(doc) = docs
                .iter()
                .find(|d| d["id"] == source["id"] && d["source_kind"] == "connector_source")
            {
                data["native_sources"][s(doc, "id")] = doc["sha256"].clone();
            }
        }
        data["claim"] = claim.clone();
        data["provider"] = provider.clone();
        data["notes"] = json!({});
        data["authority"] = json!("unreviewed_model_interpretation");
        if let Some(target) = claim["target_id"].as_str() {
            let target = targets
                .iter()
                .find(|t| s(t, "id") == target)
                .ok_or_else(|| Error::bad("Revision target disappeared"))?;
            data["target_revision"] = json!({"claim_id":target["id"],"memory_id":target["memory_id"],"revision":target["memory_revision"]});
            data["notes"][s(target, "memory_id")] = target["memory_revision"].clone();
        }
        let title = format!("{} · {}", s(claim, "subject"), s(claim, "relation"));
        let mut body = format!(
            "{}\n\nAttribution: {}. Kind: {}.",
            s(claim, "statement"),
            s(claim, "speaker"),
            s(claim, "modality")
        );
        for condition in claim["conditions"].as_array().unwrap() {
            body.push_str(&format!("\nCondition: {}", condition.as_str().unwrap()));
        }
        if !claim["valid_at"].is_null() {
            body.push_str(&format!("\nValid from: {}", s(claim, "valid_at")));
        }
        if !claim["invalid_at"].is_null() {
            body.push_str(&format!("\nValid until: {}", s(claim, "invalid_at")));
        }
        let title = graphicle_memory::text::prefix(&title, 120, 500);
        let draft = memory::save(
            db,
            identifier,
            None,
            None,
            "observation",
            &title,
            &body,
            "draft",
            &data,
        )?;
        let mut persisted = claim.clone();
        persisted["knowledge_at"] = json!(now());
        persisted["target_revision"] = data["target_revision"].clone();
        db.execute(
            "INSERT INTO am_claims VALUES(?,?,?,?,?,?,?,?)",
            params![
                claim_id,
                identifier,
                s(&draft, "id"),
                "draft",
                persisted.to_string(),
                source_state,
                now(),
                now()
            ],
        )?;
        output.push(json!({"claim_id":claim_id,"memory_id":draft["id"],"revision":draft["revision"],"state":"draft","action":claim["action"]}));
    }
    Ok(
        json!({"drafts":output,"quotes_validated":true,"semantic_entailment_verified":false,"approval_required":true}),
    )
}
pub fn apply_memory_review(
    db: &Connection,
    repository: &str,
    draft_id: &str,
    result_id: &str,
    decision: &str,
) -> Result<()> {
    let Some(mut claim) = one(
        db,
        "SELECT * FROM am_claims WHERE repository_id=? AND memory_id=?",
        params![repository, draft_id],
    )?
    else {
        return Ok(());
    };
    decode(&mut claim, "data")?;
    let mut data = claim["data"].clone();
    if decision != "approve" {
        db.execute(
            "UPDATE am_claims SET state='rejected',updated_at=? WHERE id=?",
            params![now(), s(&claim, "id")],
        )?;
        return Ok(());
    }
    let action = s(&data, "action").to_string();
    let stamp = now();
    if let Some(target) = data["target_id"].as_str() {
        let mut old = one(
            db,
            "SELECT * FROM am_claims WHERE repository_id=? AND id=? AND state='active'",
            params![repository, target],
        )?
        .ok_or_else(|| Error::conflict("Claim target is no longer current"))?;
        decode(&mut old, "data")?;
        let mut memory = one(
            db,
            "SELECT * FROM repo_memories WHERE repository_id=? AND id=?",
            params![repository, s(&old, "memory_id")],
        )?
        .ok_or_else(|| Error::conflict("Target memory disappeared"))?;
        decode(&mut memory, "data")?;
        if memory["revision"] != data["target_revision"]["revision"] {
            return Err(Error::conflict("Target revision changed before approval"));
        }
        if ["SUPERSEDE", "CORRECT", "RETRACT"].contains(&action.as_str()) {
            let mut old_data = memory["data"].clone();
            old_data["retirement"] = json!({"action":action,"by_claim":claim["id"],"knowledge_at":stamp,"valid_at":data["valid_at"]});
            memory::save(
                db,
                repository,
                Some(s(&memory, "id")),
                memory["revision"].as_i64(),
                s(&memory, "kind"),
                s(&memory, "title"),
                s(&memory, "body"),
                "retired",
                &old_data,
            )?;
            old["data"]["retired_at"] = json!(stamp);
            old["data"]["retirement_action"] = json!(action);
            if action == "SUPERSEDE" && !data["valid_at"].is_null() {
                old["data"]["invalid_at"] = data["valid_at"].clone();
            }
            db.execute(
                "UPDATE am_claims SET state='retired',data=?,updated_at=? WHERE id=?",
                params![old["data"].to_string(), stamp, s(&old, "id")],
            )?;
            db.execute(
                "INSERT INTO am_claim_events VALUES(?,?,?,?,?,?)",
                params![
                    id(),
                    repository,
                    s(&old, "id"),
                    "RETIRE",
                    old["data"].to_string(),
                    stamp
                ],
            )?;
        }
        // Target history remains provenance, without a live freshness dependency
        // on a fact that this revision deliberately superseded.
        if let Some(mut memory) = one(
            db,
            "SELECT * FROM repo_memories WHERE repository_id=? AND id=?",
            params![repository, result_id],
        )? {
            decode(&mut memory, "data")?;
            if let Some(notes) = memory["data"]["notes"].as_object_mut() {
                notes.remove(s(&old, "memory_id"));
            }
            memory["data"]["historical_target"] = data["target_revision"].clone();
            db.execute(
                "UPDATE repo_memories SET data=? WHERE id=?",
                params![memory["data"].to_string(), result_id],
            )?;
            let mut journal = memory.clone();
            journal["data"] = json!(memory["data"].to_string());
            db.execute(
                "UPDATE repo_memory_revisions SET data=? WHERE memory_id=? AND revision=?",
                params![journal.to_string(), result_id, memory["revision"].as_i64()],
            )?;
        }
    }
    data["approved_at"] = json!(stamp);
    let state = if ["KEEP", "CONFIRM", "RETRACT"].contains(&action.as_str()) {
        "event"
    } else {
        "active"
    };
    db.execute(
        "UPDATE am_claims SET memory_id=?,state=?,data=?,updated_at=? WHERE id=?",
        params![result_id, state, data.to_string(), stamp, s(&claim, "id")],
    )?;
    db.execute(
        "INSERT INTO am_claim_events VALUES(?,?,?,?,?,?)",
        params![
            id(),
            repository,
            s(&claim, "id"),
            action,
            data.to_string(),
            stamp
        ],
    )?;
    if state == "active" {
        let mut entities = Vec::new();
        for facet in ["subject", "object"] {
            let name = s(&data, facet);
            let provenance = data["evidence"]
                .as_array()
                .unwrap()
                .first()
                .map(|v| v["id"].clone());
            let entity_id = digest(
                &json!({"repository":repository,"name":name,"speaker":data["speaker"],"source":provenance}),
            );
            db.execute(
                "INSERT OR IGNORE INTO am_entities VALUES(?,?,?,?,?,?)",
                params![
                    entity_id,
                    repository,
                    name,
                    "source_scoped",
                    json!({"speaker":data["speaker"],"source":provenance}).to_string(),
                    stamp
                ],
            )?;
            entities.push(entity_id);
        }
        db.execute(
            "INSERT OR REPLACE INTO am_entity_links VALUES(?,?,?,?)",
            params![repository, s(&claim, "id"), entities[0], entities[1]],
        )?;
    }
    Ok(())
}
pub fn prepare(db: &Connection, repository: &Value, p: &Principal, args: &Value) -> Result<Value> {
    let docs = memory::documents(db, repository, p)?;
    let limit = crate::integer(args, "limit", 4, 12)?.max(1);
    // Any byte of a document: an indexed file or a converted or connector
    // document (earlier builds: 512 KiB).
    let limits = crate::limits::current();
    let start = crate::integer(
        args,
        "start_byte",
        0,
        limits.file_bytes.max(limits.document_text_bytes),
    )?;
    let mut sources = Vec::new();
    for doc in docs
        .iter()
        .filter(|d| args["source_id"].as_str().is_none_or(|id| s(d, "id") == id))
        .take(limit)
    {
        let read = memory::read_source(doc, &json!({"start_byte":start,"line_count":100}))?;
        let text = graphicle_memory::text::prefix(s(&read, "quote"), 1500, 8000);
        sources.push(json!({"id":doc["id"],"text":text,"sha256":doc["sha256"],"recorded_at":doc["recorded_at"],"scope":repository["id"],"start_byte":read["byte_start"],"next_byte":read["byte_start"].as_u64().unwrap_or(0)+text.len() as u64}));
    }
    if sources.is_empty() {
        return Err(Error::bad("No authorized sources to interpret"));
    }
    let targets = current(db, repository, p)?
        .into_iter()
        .filter(|t| {
            t["data"]["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| sources.iter().any(|d| d["id"] == e["id"]))
        })
        .take(12)
        .collect::<Vec<_>>();
    let context = json!({"sources":sources,"source_state":memory::state(&docs),"targets":targets,"scope":repository["id"]});
    Ok(
        json!({"context":context,"sha256":digest(&context),"contract":native("interpretation.contract",json!({}))?,"model_calls":0}),
    )
}
impl Store {
    pub fn interpretation(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        propose_draft: bool,
    ) -> Result<Value> {
        self.authorized(
            workspace,
            token,
            if propose_draft { "propose" } else { "read" },
            |db, p| {
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                let prepared = prepare(db, &repository, p, args)?;
                if !propose_draft {
                    return Ok(prepared);
                }
                if args["context_sha256"] != prepared["sha256"] {
                    return Err(Error::conflict(
                        "Interpretation context changed; prepare it again",
                    ));
                }
                propose(
                    db,
                    &repository,
                    p,
                    &args["claims"],
                    prepared["context"]["sources"].as_array().unwrap(),
                    prepared["context"]["targets"].as_array().unwrap(),
                    &json!({"kind":"caller_supplied","model_calls":0}),
                )
            },
        )
    }

    pub fn claim_history(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let memories = memory::list(db, &repository, p, true)?;
            let mut claims = Vec::new();
            let as_of = args["as_of"]
                .as_str()
                .map(graphicle_memory::temporal::instant)
                .transpose()
                .map_err(Error::bad)?;
            let known_at = args["known_at"]
                .as_str()
                .map(graphicle_memory::temporal::instant)
                .transpose()
                .map_err(Error::bad)?;
            for mut row in rows(
                db,
                "SELECT * FROM am_claims WHERE repository_id=? ORDER BY created_at,id",
                [s(&repository, "id")],
            )? {
                if !memories.iter().any(|m| m["id"] == row["memory_id"]) {
                    continue;
                }
                decode(&mut row, "data")?;
                // Reconstruct the knowledge view from immutable events. The
                // current row may contain an invalidation learned much later.
                if let Some(at) = known_at {
                    let mut selected=None;
                    for mut event in rows(db,"SELECT action,data,created_at FROM am_claim_events WHERE repository_id=? AND claim_id=? ORDER BY created_at,rowid",params![s(&repository,"id"),s(&row,"id")])? {
                        if graphicle_memory::temporal::instant(s(&event,"created_at")).map_err(Error::bad)?<=at {
                            decode(&mut event,"data")?;selected=Some(event);
                        }
                    }
                    let Some(event)=selected else {continue;};
                    row["data"]=event["data"].clone();
                    row["state"]=json!(if event["action"]=="RETIRE" {"retired"} else if ["KEEP","CONFIRM","RETRACT"].contains(&s(&event,"action")) {"event"} else {"active"});
                    row["updated_at"]=event["created_at"].clone();
                }
                let data=&row["data"];
                if (known_at.is_some() || as_of.is_some()) && data["approved_at"].is_null() {continue;}
                if (known_at.is_some() || as_of.is_some()) && row["state"]=="retired" && data["retirement_action"]!="SUPERSEDE" {continue;}
                if let Some(at)=as_of {
                    let lower=data["valid_at"].as_str().map(graphicle_memory::temporal::instant).transpose().map_err(Error::bad)?;
                    let upper=data["invalid_at"].as_str().map(graphicle_memory::temporal::instant).transpose().map_err(Error::bad)?;
                    if lower.is_some_and(|t|t>at) || upper.is_some_and(|t|t<=at) {continue;}
                }
                claims.push(row);
            }
            Ok(json!({"claims":claims,"unknown_dates_are_not_inferred":true}))
        })
    }
}
