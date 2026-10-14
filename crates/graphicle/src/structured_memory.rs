//! Structured recall over the existing reviewed-memory journal and source ACLs.
//! No second store, implicit approval, model call or benchmark-specific policy.
use crate::{
    Error, Result, memory, now, query,
    store::{Store, decode, repo, rows, s},
    string,
};
use graphicle_memory::recall::{self, Metadata, Query, Record};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub fn metadata(value: &Value) -> Result<Metadata> {
    let value: Metadata = serde_json::from_value(value.clone())
        .map_err(|e| Error::bad(format!("Invalid recall metadata: {e}")))?;
    value.validate().map_err(Error::bad)?;
    Ok(value)
}

/// Only a member-approved note (or the existing explicit review workflow)
/// can introduce trusted recall metadata. An edit that omits it preserves it.
pub fn attach(db: &Connection, repository: &str, args: &Value, data: &mut Value) -> Result<()> {
    if let Some(value) = args.get("recall") {
        data["recall"] = serde_json::to_value(metadata(value)?)?;
    } else if let Some(identifier) = args["memory_id"].as_str()
        && let Some(mut prior) = crate::store::one(
            db,
            "SELECT data FROM repo_memories WHERE repository_id=? AND id=?",
            params![repository, identifier],
        )?
    {
        decode(&mut prior, "data")?;
        if let Some(value) = prior["data"].get("recall") {
            data["recall"] = serde_json::to_value(metadata(value)?)?;
        }
    }
    Ok(())
}

/// Validate both records before writing either. Replacement is explicit and
/// revision-checked; ingestion order never decides which statement is current.
pub fn replacement(args: &Value, data: &mut Value, memories: &[Value]) -> Result<Option<Value>> {
    let Some(spec) = args.get("supersedes") else {
        return Ok(None);
    };
    let target = string(spec, "memory_id", 100)?;
    if args["memory_id"].as_str() == Some(target) {
        return Err(Error::bad("A memory cannot supersede itself"));
    }
    let old = memories
        .iter()
        .find(|m| s(m, "id") == target && m["state"] == "reviewed" && m["stale"] != true)
        .ok_or_else(|| Error::bad("Superseded memory is not current and accessible"))?;
    let expected = spec["expected_revision"]
        .as_i64()
        .ok_or_else(|| Error::bad("Supersession requires expected_revision"))?;
    if old["revision"].as_i64() != Some(expected) {
        return Err(Error::conflict("Superseded memory changed"));
    }
    let new = metadata(&data["recall"])?;
    let prior = metadata(&old["data"]["recall"])?;
    let scope = |m: &Metadata| {
        m.scopes
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    if scope(&new) != scope(&prior)
        || new.state != recall::State::Active
        || prior.state != recall::State::Active
        || prior.superseded_by.is_some()
        || new.superseded_by.is_some()
    {
        return Err(Error::bad(
            "Supersession requires active memories in the same scope",
        ));
    }
    let at = new
        .valid_from
        .as_deref()
        .ok_or_else(|| Error::bad("Supersession requires an explicit valid_from"))?;
    let at = graphicle_memory::temporal::instant(at).map_err(Error::bad)?;
    for (field, is_start) in [(&prior.valid_from, true), (&prior.valid_to, false)] {
        if let Some(value) = field {
            let value = graphicle_memory::temporal::instant(value).map_err(Error::bad)?;
            if (is_start && value >= at) || (!is_start && value < at) {
                return Err(Error::bad(
                    "Supersession is outside the target validity interval",
                ));
            }
        }
    }
    data["historical_target"] = json!({"memory_id":target,"revision":expected});
    Ok(Some(old.clone()))
}

pub fn close_replaced(db: &Connection, repository: &str, old: &Value, new: &Value) -> Result<()> {
    let mut data = old["data"].clone();
    let mut recall = metadata(&data["recall"])?;
    recall.valid_to = metadata(&new["data"]["recall"])?.valid_from;
    recall.superseded_by = Some(s(new, "id").to_owned());
    recall.validate().map_err(Error::bad)?;
    data["recall"] = serde_json::to_value(recall)?;
    // Preserve reviewed status. The interval withholds a future-dated
    // replacement until it takes effect, and keeps the old fact before then.
    memory::save(
        db,
        repository,
        Some(s(old, "id")),
        old["revision"].as_i64(),
        s(old, "kind"),
        s(old, "title"),
        s(old, "body"),
        "reviewed",
        &data,
    )?;
    Ok(())
}

/// Current context uses the same validity/scope gate as explicit memory search.
/// Ordinary unstructured notes retain their existing retrieval behavior.
pub fn context_allows(
    memory: &Value,
    args: &Value,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let Some(value) = memory["data"].get("recall") else {
        return Ok(true);
    };
    let query: Query = serde_json::from_value(json!({
        "question":"", "scopes":args["memory_scopes"], "as_of":args["memory_as_of"]
    }))
    .map_err(|e| Error::bad(format!("Invalid memory query: {e}")))?;
    query.validate().map_err(Error::bad)?;
    Ok(recall::eligibility(&metadata(value)?, &query, at)
        .map_err(Error::bad)?
        .is_none())
}

impl Store {
    pub fn memory_search(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let request: Query = serde_json::from_value(args.clone())
            .map_err(|e| Error::bad(format!("Invalid memory query: {e}")))?;
        request.validate().map_err(Error::bad)?;
        let known = match args.get("known_at").filter(|v| !v.is_null()) {
            Some(value) => Some(
                graphicle_memory::temporal::instant(
                    value
                        .as_str()
                        .ok_or_else(|| Error::bad("known_at must be RFC3339"))?,
                )
                .map_err(Error::bad)?,
            ),
            None => None,
        };
        self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let documents = memory::documents(db, &repository, p)?;
            let visible = memory::list_of(db, &repository, &documents, true)?;
            let mut memories = Vec::new();
            let mut descriptors = Vec::new();
            let mut evidence_memories = Vec::new();
            let mut withheld = 0;
            let at = graphicle_memory::temporal::instant(&now()).map_err(Error::bad)?;
            for current in &visible {
                // Knowledge time chooses a revision, not an earlier trust or
                // access decision. Withdrawn or stale current memories must
                // stay unavailable even when their journal has reviewed text.
                if current["state"] != "reviewed" || current["stale"] == true {
                    withheld += 1;
                    continue;
                }
                let selected = if let Some(known) = known {
                    let mut selected = None;
                    for mut revision in rows(db,
                        "SELECT data,created_at FROM repo_memory_revisions WHERE memory_id=? ORDER BY revision",
                        [s(current, "id")])? {
                        if graphicle_memory::temporal::instant(s(&revision, "created_at")).map_err(Error::bad)? <= known {
                            decode(&mut revision, "data")?;
                            let mut value = revision["data"].clone();
                            decode(&mut value, "data")?;
                            selected = Some(value);
                        }
                    }
                    selected
                } else { Some(current.clone()) };
                let Some(mut selected) = selected else { continue };
                // list_of has already checked current access and calculated
                // transitive freshness for every memory in one batch. Only a
                // historical revision needs its own dependency revalidation.
                let unavailable = if known.is_some() {
                    !memory::accessible(&selected["data"], &documents, &visible)
                        || !memory::reasons(
                            db,
                            s(&repository, "id"),
                            Some(s(current, "id")),
                            &selected["data"],
                            &documents,
                            &visible,
                        )?
                        .is_empty()
                } else { selected["stale"] == true };
                if selected["state"] != "reviewed" || unavailable {
                    withheld += 1;
                    continue;
                }
                selected["stale"] = json!(false);
                let Some(value) = selected["data"].get("recall") else {
                    evidence_memories.push(selected);
                    continue;
                };
                let descriptor = metadata(value)?;
                if recall::eligibility(&descriptor, &request, at).map_err(Error::bad)?.is_none() {
                    evidence_memories.push(selected.clone());
                }
                descriptors.push(descriptor);
                memories.push(selected);
            }
            let records: Vec<_> = memories.iter().zip(&descriptors).map(|(m, metadata)|
                Record { id: s(m, "id"), text: s(m, "body"), metadata }).collect();
            let selection = recall::select(&records, &request, at).map_err(Error::bad)?;
            let by_id: std::collections::BTreeMap<_, _> = memories.iter().map(|m| (s(m,"id"), m)).collect();
            let selected: Vec<Value> = selection.hits.iter().map(|hit| -> Result<Value> {
                let mut memory = by_id[hit.id.as_str()].clone();
                memory["match"] = serde_json::to_value(hit)?;
                Ok(memory)
            }).collect::<Result<_>>()?;
            let snapshot = query::Snapshot { repository, state: memory::state(&documents),
                documents, memories: evidence_memories, graph: json!({"edges":[]}) };
            let sources = crate::memory_evidence::gather(&snapshot, &selected)?;
            let windows: Vec<_> = sources.hits.iter().map(|hit|
                json!({"id":hit["source"]["id"],"text":hit["text"]})).collect();
            let support = sources.coverage(&windows);
            Ok(json!({"memories":selected,"selection":selection,"evidence":sources.hits,
                "memory_support":support,
                "withheld_unreviewed_or_stale":withheld,"match_mode":request.match_mode,
                "as_of":request.as_of,"known_at":args["known_at"],"model_calls":0,
                "semantic_entailment_verified":false}))
        })
    }
}
