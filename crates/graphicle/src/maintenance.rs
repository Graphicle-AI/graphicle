//! Source removal and local backups share the same persistence boundary.
//! Forgetting (connector.delete, source.forget) is implemented and verified in
//! [`crate::deletion`].
use crate::deletion::{self, Scope};
use crate::store::{self, Store, decode, one, repo, rows, s};
use crate::{Error, Result, integer, now, string};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

/// Rows of `sql` still present for each key (the repository binds first,
/// then the key's columns), summed.
fn still_present(
    db: &Connection,
    sql: &str,
    repository: &str,
    keys: impl Iterator<Item = Vec<String>>,
) -> Result<i64> {
    let mut total = 0;
    for key in keys {
        total += db.query_row(
            sql,
            rusqlite::params_from_iter(std::iter::once(repository.to_string()).chain(key)),
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(total)
}

/// Delete one project: queue the cleanup of its published projections,
/// forget its dependency-index entries, purge its graph cache, delete its
/// rows (they cascade) and sign its receipt, with `removed` added to the
/// receipt's `removed`.
fn delete_project(
    db: &Connection,
    p: &store::Principal,
    identifier: &str,
    mut removed: Value,
) -> Result<Value> {
    let connectors: Vec<String> = rows(
        db,
        "SELECT id FROM am_connectors WHERE repository_id=? ORDER BY id",
        [identifier],
    )?
    .iter()
    .map(|c| s(c, "id").to_string())
    .collect();
    crate::projection::enqueue(db, identifier, "project_deleted")?;
    removed["references"] =
        deletion::forget_project_references(db, &p.workspace, identifier, &connectors)?;
    if let Some(cache) = crate::graph_projection::cache_root(db, identifier) {
        crate::graph_materialize::purge(&cache)?;
    }
    db.execute("DELETE FROM repo_projects WHERE id=?", [identifier])?;
    graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
    deletion::project_receipt(db, p, identifier, &connectors, removed)
}

/// `retention.apply` with `sources`: forget the documents the policy selects
/// through certified deletion (plan, forget, erase, verify), one receipt per
/// batch of up to 1,000 documents, each carrying the policy. The caller must
/// echo the `plan_hash` of `retention.preview`, so exactly the previewed
/// documents are forgotten.
fn apply_source_retention(
    db: &Connection,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let policy = crate::retention::SourcePolicy::from_args(args)?;
    let plan = crate::retention::source_plan(db, p, repository, &policy, chrono::Utc::now())?;
    if args["plan_hash"].as_str() != Some(plan.plan_hash.as_str()) {
        return Err(Error::conflict(
            "The retention plan changed or was not previewed; run retention.preview and pass its plan_hash",
        ));
    }
    let policy_value = json!(policy);
    let mut receipts = Vec::new();
    let mut forgotten_documents = 0usize;
    for (connector, documents) in &plan.documents {
        let listed: Vec<&String> = documents.iter().collect();
        for batch in listed.chunks(deletion::MAX_SCOPE_DOCUMENTS) {
            let scope =
                Scope::document_set(connector, batch.iter().map(|d| (*d).clone()).collect());
            let started = now();
            let forget_plan = deletion::plan(db, &p.workspace, identifier, &scope)?;
            if forget_plan.sources() == 0 {
                continue;
            }
            crate::projection::enqueue(db, identifier, "source_forgotten")?;
            let since = forget_plan.since().map(str::to_string);
            let forgotten =
                deletion::forget(db, &p.workspace, identifier, &scope, since.as_deref())?;
            let erased = deletion::erase(db, identifier, &scope)?;
            graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
            store::event(
                db,
                identifier,
                "source_forgotten",
                json!({"derived_records_removed":forgotten["memories"],"retention":true}),
            )?;
            forgotten_documents += batch.len();
            receipts.push(deletion::policy_receipt(
                db,
                p,
                repository,
                "retention.forget",
                forget_plan,
                &started,
                forgotten,
                erased,
                Some(&policy_value),
            )?);
        }
    }
    Ok(json!({
        "forgotten_documents": forgotten_documents,
        "plan_hash": plan.plan_hash,
        "cutoff": plan.cutoff,
        "receipts": receipts,
        "receipt": receipts.last().cloned().unwrap_or(Value::Null),
        "local_backups_are_separate": true,
    }))
}

impl Store {
    pub fn maintain(
        &self,
        workspace: &str,
        token: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let mut result = self.authorized(workspace,token,"admin",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let identifier=s(&repository,"id");
            match operation {
                "connector.state" | "connector.delete" | "source.forget"=>{
                    let connector=string(args,"connector_id",100)?;
                    one(db,"SELECT id FROM am_connectors WHERE repository_id=? AND id=?",params![identifier,connector])?.ok_or_else(||Error::bad("Connector unavailable"))?;
                    if operation!="connector.state" {
                        let scope=if operation=="connector.delete" {Scope::connector(connector)} else {Scope::documents(connector,&args["document_ids"])?};
                        // Capture coverage first; forget, erase, verify and sign in one transaction.
                        let started=now();
                        let plan=deletion::plan(db,&p.workspace,identifier,&scope)?;
                        if operation=="source.forget" && plan.sources()==0 {return Err(Error::bad("None of these documents exist in this connector"));}
                        crate::projection::enqueue(db,identifier,if operation=="connector.delete" {"connector_deleted"} else {"source_forgotten"})?;
                        let since=plan.since().map(str::to_string);
                        let forgotten=deletion::forget(db,&p.workspace,identifier,&scope,since.as_deref())?;
                        let erased=deletion::erase(db,identifier,&scope)?;
                        // In-process features and layouts of forgotten text go now, not at expiry.
                        graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
                        let derived=forgotten["memories"].clone();
                        store::event(db,identifier,"source_forgotten",json!({"derived_records_removed":derived}))?;
                        let receipt=deletion::receipt(db,p,&repository,operation,plan,&started,forgotten,erased)?;
                        return Ok(json!({"deleted":true,"derived_records_removed":derived,"local_backups_are_separate":true,"receipt":receipt}));
                    }
                    let state=string(args,"state",20)?;
                    if !["active","paused"].contains(&state){return Err(Error::bad("Activate or pause this connector"));}
                    db.execute("UPDATE am_connectors SET state=?,generation=generation+1,cursor=NULL,last_sync=NULL WHERE id=?",params![state,connector])?;
                    db.execute("DELETE FROM am_connector_stage WHERE connector_id=?",[connector])?;
                    db.execute("DELETE FROM am_model_cache WHERE repository_id=?",[identifier])?;
                    store::invalidate(db,identifier)?;
                    Ok(json!({"state":state,"remote_sources_withheld_until_sync":state=="active"}))
                },
                "project.delete"=>{
                    if args["expected_snapshot"]!=repository["snapshot"] {return Err(Error::conflict("Project snapshot changed; inspect it before deleting"));}
                    // The PR scratch projects `graphicle pr` keeps for this
                    // project hold copies of its code: they go
                    // first, each with its own receipt, and this project's
                    // receipt names them.
                    let copies=crate::folders::scratch_projects(db,identifier)?;
                    let mut scratch=Vec::with_capacity(copies.len());
                    for copy in &copies {
                        scratch.push(delete_project(db,p,copy,json!({}))?);
                    }
                    let removed=if copies.is_empty() {json!({})} else {json!({"pr_scratch_projects":copies})};
                    let receipt=delete_project(db,p,identifier,removed)?;
                    let mut result=json!({"deleted":true,"local_backups_are_separate":true,"receipt":receipt});
                    if !scratch.is_empty() {result["pr_scratch_projects"]=json!(scratch);}
                    Ok(result)
                },
                "storage.prune"=>{
                    let keep=integer(args,"keep_snapshots",20,1000)?.max(1);
                    let snapshots=rows(db,"SELECT sha256,manifest FROM repo_snapshots WHERE repository_id=? ORDER BY created_at DESC,sha256",[identifier])?;
                    let mut keep_snapshots:BTreeSet<String>=snapshots.iter().take(keep).map(|r|s(r,"sha256").to_string()).collect();
                    keep_snapshots.insert(s(&repository,"snapshot").to_string());
                    for run in rows(db,"SELECT start_snapshot,end_snapshot FROM repo_runs WHERE repository_id=?",[identifier])? {
                        for key in ["start_snapshot","end_snapshot"] {if let Some(v)=run[key].as_str(){keep_snapshots.insert(v.to_string());}}
                    }
                    let mut objects:BTreeSet<String>=store::heads(db,identifier)?.into_values().collect();
                    let mut pruned:Vec<String>=Vec::new();
                    let (mut snapshots_removed,mut objects_removed,mut originals_removed)=(Vec::new(),Vec::new(),Vec::new());
                    for snapshot in &snapshots {
                        if keep_snapshots.contains(s(snapshot,"sha256")) {
                            let manifest:Value=serde_json::from_str(s(snapshot,"manifest"))?;
                            for value in manifest.as_object().into_iter().flat_map(|o|o.values()) {if let Some(sha)=value["sha256"].as_str(){objects.insert(sha.to_string());}}
                        } else {
                            db.execute("DELETE FROM repo_snapshots WHERE repository_id=? AND sha256=?",params![identifier,s(snapshot,"sha256")])?;
                            snapshots_removed.push(s(snapshot,"sha256").to_string());
                        }
                    }
                    for revision in rows(db,"SELECT r.data FROM repo_memory_revisions r JOIN repo_memories m ON m.id=r.memory_id WHERE m.repository_id=?",[identifier])? {
                        let mut memory:Value=serde_json::from_str(s(&revision,"data"))?;decode(&mut memory,"data")?;
                        for sha in memory["data"]["dependencies"].as_object().into_iter().flat_map(|o|o.values()).filter_map(Value::as_str){objects.insert(sha.to_string());}
                    }
                    let mut freed=0;
                    for object in rows(db,"SELECT sha256,bytes FROM repo_objects WHERE repository_id=?",[identifier])? {
                        if !objects.contains(s(&object,"sha256")){freed+=object["bytes"].as_u64().unwrap_or(0);db.execute("DELETE FROM repo_objects WHERE repository_id=? AND sha256=?",params![identifier,s(&object,"sha256")])?;objects_removed.push(s(&object,"sha256").to_string());}
                    }
                    let syncs_removed=db.execute("DELETE FROM repo_syncs WHERE repository_id=? AND state NOT IN ('pending','preparing')",[identifier])?;
                    let mut originals_freed=0;
                    for original in rows(db,"SELECT source_id,sha256,text_sha256,length(bytes) AS bytes FROM am_original_documents WHERE repository_id=?",[identifier])? {
                        // Keep originals referenced by any retained source object, including snapshots.
                        let current=one(db,"SELECT 1 AS yes FROM am_connector_docs WHERE connector_id IN (SELECT id FROM am_connectors WHERE repository_id=?) AND 'connector:'||connector_id||':'||id=? AND present=1 AND json_extract(metadata,'$.provider_metadata.original_sha256')=?",params![identifier,s(&original,"source_id"),s(&original,"sha256")])?.is_some();
                        if !current && !objects.contains(s(&original,"text_sha256")){
                            originals_freed+=original["bytes"].as_u64().unwrap_or(0);db.execute("DELETE FROM am_original_documents WHERE repository_id=? AND source_id=? AND sha256=?",params![identifier,s(&original,"source_id"),s(&original,"sha256")])?;
                            originals_removed.push((s(&original,"source_id").to_string(),s(&original,"sha256").to_string()));
                        }
                    }
                    let mut result=json!({"bytes_reclaimed":freed+originals_freed,"original_bytes_reclaimed":originals_freed,"kept_snapshots":keep_snapshots.len(),"memory_revisions_preserved":true});
                    if !(snapshots_removed.is_empty() && objects_removed.is_empty() && originals_removed.is_empty()) {
                        // Re-count what was pruned, then sign: no silent deletion.
                        let remaining=json!({
                            "repo_snapshots":still_present(db,"SELECT count(*) FROM repo_snapshots WHERE repository_id=? AND sha256=?",identifier,snapshots_removed.iter().map(|k|vec![k.clone()]))?,
                            "repo_objects":still_present(db,"SELECT count(*) FROM repo_objects WHERE repository_id=? AND sha256=?",identifier,objects_removed.iter().map(|k|vec![k.clone()]))?,
                            "am_original_documents":still_present(db,"SELECT count(*) FROM am_original_documents WHERE repository_id=? AND source_id=? AND sha256=?",identifier,originals_removed.iter().map(|(a,b)|vec![a.clone(),b.clone()]))?,
                        });
                        pruned.extend(snapshots_removed.iter().map(|k|format!("snapshot:{k}")));
                        pruned.extend(objects_removed.iter().map(|k|format!("object:{k}")));
                        pruned.extend(originals_removed.iter().map(|(a,b)|format!("original:{a}@{b}")));
                        let removed=json!({"repo_snapshots":snapshots_removed.len(),"repo_objects":objects_removed.len(),"repo_syncs":syncs_removed,"am_original_documents":originals_removed.len(),"bytes":freed+originals_freed});
                        result["receipt"]=deletion::rows_receipt(db,p,identifier,"storage.prune","storage",json!({"keep_snapshots":keep}),removed,&pruned,remaining,false)?;
                    }
                    Ok(result)
                },
                "retention.apply"=>{
                    if args.get("sources").is_some_and(|v|!v.is_null()) {
                        return apply_source_retention(db,p,&repository,args);
                    }
                    let retention=crate::retention::Retention::from_args(args)?;
                    let mut result=crate::retention::apply(db,identifier,&retention)?;
                    let removed=result["deleted"].clone();
                    if removed.as_object().is_some_and(|counts|counts.values().any(|n|n.as_u64().unwrap_or(0)>0)) {
                        let remaining=crate::retention::remaining(db,identifier,&retention)?;
                        result["receipt"]=deletion::rows_receipt(db,p,identifier,"retention.graph","graph_by_products",json!(retention),removed,&[],remaining,false)?;
                    }
                    Ok(result)
                },
                _=>Err(Error::bad("Unknown maintenance operation"))
            }
        })?;
        if result.get("receipt").is_some() {
            // After commit, flush the deleted pages' old images out of the WAL.
            result["storage"] = deletion::checkpoint(self)?;
        }
        Ok(result)
    }
    pub fn backup(&self, path: &Path) -> Result<Value> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        drop(file);
        let result = (|| {
            self.connect()?.backup(rusqlite::MAIN_DB, path, None)?;
            let backup = Connection::open(path)?;
            // A portable source backup must not silently contain unusable encrypted
            // credentials or require copying the device's master key beside it.
            backup.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; BEGIN IMMEDIATE; UPDATE am_connectors SET state='paused',last_sync=NULL,cursor=NULL,generation=generation+1 WHERE credential_env LIKE 'secret:%' OR credential_env LIKE 'oauth:%'; UPDATE am_projection_cleanup SET state='paused',lease_token=NULL,lease_until=NULL WHERE state!='complete'; UPDATE am_jobs SET state='cancelled' WHERE state IN ('queued','retry','running'); DELETE FROM am_secrets; COMMIT;")?;
            // Sessions, provisioning tokens and paid entitlements are scrubbed
            // on any store that has the team and commerce tables, whichever
            // edition takes the backup; then installed extensions scrub their
            // own tables, before the copy is compacted.
            crate::store_compat::scrub_backup(&backup)?;
            crate::extension::installed().scrub_backup(&backup)?;
            backup.execute_batch("VACUUM;")?;
            let check: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if check != "ok" {
                return Err(Error::bad("Backup integrity verification failed"));
            }
            Ok(
                json!({"path":path,"created_at":now(),"integrity":"ok","contains_private_sources":true,"provider_credentials_included":false,"reconnect_accounts_after_restore":true}),
            )
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}
