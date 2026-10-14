//! Project operations shared by the HTTP and stdio surfaces.
use crate::{Error, Result, integer, now, string};
use crate::{
    memory,
    store::{self, Store, decode, one, repo, rows, s},
};
use rusqlite::params;
use serde_json::{Value, json};

impl Store {
    pub fn projects(&self, workspace: &str, token: &str) -> Result<Value> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let mut visible = Vec::new();
            // A PR scratch copy says so; other rows are unchanged.
            let scratch = crate::graph_projection::pr_scratch_projects(db, workspace)?;
            for mut project in rows(db, "SELECT id,project_key,name,snapshot,generation,created_at,updated_at FROM repo_projects WHERE workspace_id=? ORDER BY updated_at DESC", [workspace])? {
                if crate::graph_projection::repo_readable(db, p, s(&project, "id"))? {
                    if scratch.contains(s(&project, "id")) {
                        project["role"] = json!(crate::graph_projection::PR_SCRATCH);
                        // The folder project `graphicle pr`
                        // keeps this scratch project for.
                        if let Some(of) = crate::folders::scratch_of(db, s(&project, "id"))? {
                            project["scratch_of"] = json!(of);
                        }
                    }
                    visible.push(project);
                }
            }
            Ok(json!(visible))
        })
    }
    /// `project.list` (MCP `list_projects` on workspace
    /// connections): the projects of the workspace the caller may read, in
    /// the order workspace recall reads them (creation order), without PR
    /// scratch projects unless `include_scratch` is true; `limit` of them
    /// (100 by default, at most 500) from `offset`, as `files` pages. Ids,
    /// names and times only: a project's key can carry a folder path or, in
    /// the SDK's `Memory`, a user id. A project the caller may not read is
    /// not listed or counted.
    pub fn project_list(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        if args
            .get("include_scratch")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(Error::bad("include_scratch must be true or false"));
        }
        let include_scratch = args["include_scratch"] == true;
        let offset = integer(args, "offset", 0, 100000)?;
        let limit = integer(args, "limit", 100, 500)?.max(1);
        self.authorized_read(workspace, token, "read", |db, p| {
            let scratch = crate::graph_projection::pr_scratch_projects(db, workspace)?;
            let mut listed = Vec::new();
            let mut total = 0usize;
            let mut recalled = 0usize;
            for mut project in rows(
                db,
                "SELECT id,name,snapshot,created_at,updated_at FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id",
                [workspace],
            )? {
                let id = s(&project, "id").to_string();
                if !crate::graph_projection::repo_readable(db, p, &id)? {
                    continue;
                }
                let is_scratch = scratch.contains(&id);
                if is_scratch && !include_scratch {
                    continue;
                }
                if !is_scratch {
                    recalled += 1;
                }
                if total >= offset && listed.len() < limit {
                    if is_scratch {
                        project["role"] = json!(crate::graph_projection::PR_SCRATCH);
                        if let Some(of) = crate::folders::scratch_of(db, &id)? {
                            project["scratch_of"] = json!(of);
                        }
                    }
                    listed.push(project);
                }
                total += 1;
            }
            Ok(json!({"projects":listed,"total":total,
                "next_offset":if offset + limit < total { Some(offset + limit) } else { None },
                "workspace_recall":{"projects":recalled.min(crate::workspace::FANOUT),"readable":recalled,
                    "max_projects":crate::workspace::FANOUT}}))
        })
    }
    pub fn project_read(
        &self,
        workspace: &str,
        token: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        self.authorized_read(workspace,token,"read",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let identifier=s(&repository,"id");
            match operation {
                "files"=>{
                    let prefix=args["prefix"].as_str().unwrap_or("");let offset=integer(args,"offset",0,100000)?;let limit=integer(args,"limit",100,500)?.max(1);
                    let docs=memory::documents(db,&repository,p)?;let matching:Vec<_>=docs.iter().filter(|d|s(d,"path").starts_with(prefix)).collect();
                    let files:Vec<_>=matching.iter().skip(offset).take(limit).map(|d|json!({"id":d["id"],"path":d["path"],"title":d["title"],"sha256":d["sha256"],"source_kind":d["source_kind"],"bytes":s(d,"content").len()})).collect();
                    Ok(json!({"files":files,"total":matching.len(),"next_offset":if offset+limit<matching.len(){Some(offset+limit)}else{None},"snapshot":repository["snapshot"]}))
                },
                "memories"=>Ok(json!(memory::list(db,&repository,p,args["include_retired"]==true)?)),
                "memory.history"=>{
                    let memories=memory::list(db,&repository,p,true)?;let memory_id=string(args,"memory_id",100)?;
                    if !memories.iter().any(|m|s(m,"id")==memory_id){return Err(Error::bad("Memory unavailable"));}
                    let known=args["known_at"].as_str().map(graphicle_memory::temporal::instant).transpose().map_err(Error::bad)?;
                    let mut versions=Vec::new();
                    let docs=memory::documents(db,&repository,p)?;
                    for mut row in rows(db,"SELECT data,created_at FROM repo_memory_revisions WHERE memory_id=? ORDER BY revision",[memory_id])? {
                        if known.is_some_and(|at|graphicle_memory::temporal::instant(s(&row,"created_at")).is_ok_and(|t|t>at)){continue;}
                        decode(&mut row,"data")?;let mut memory=row["data"].clone();decode(&mut memory,"data")?;
                        if memory::accessible(&memory["data"],&docs,&memories){versions.push(memory);}
                    }
                    if known.is_some(){Ok(json!({"memory":versions.last(),"historical":true}))}else{Ok(json!({"versions":versions}))}
                },
                "memory.export"=>Ok(json!({"format":"graphicle.memories.v1","repository_id":identifier,"memories":memory::list(db,&repository,p,true)?})),
                "history"=>Ok(json!({"snapshots":rows(db,"SELECT sha256,metadata,created_at FROM repo_snapshots WHERE repository_id=? ORDER BY created_at DESC LIMIT 100",[identifier])?})),
                _=>Err(Error::bad("Unsupported project read"))
            }
        })
    }
    pub fn project_mutate(
        &self,
        workspace: &str,
        token: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let permission = match operation {
            "learning.refresh.propose" | "memory.import" | "memory.propose" => "propose",
            "sync.abort" => "collect",
            _ => "approve",
        };
        let mut result = self.authorized(workspace,token,permission,|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let identifier=s(&repository,"id");
            match operation {
                "sync.abort"=>{db.execute("UPDATE repo_syncs SET state='aborted' WHERE repository_id=? AND id=? AND state IN ('pending','preparing')",params![identifier,string(args,"sync_id",100)?])?;Ok(json!({"aborted":true}))},
                "learning.refresh.propose"=>memory::propose_refresh(db,&repository,p,args),
                "memory.state"=>{
                    let memories=memory::list(db,&repository,p,true)?;let old=memories.iter().find(|m|m["id"]==args["memory_id"]).ok_or_else(||Error::bad("Memory unavailable"))?;
                    if args["state"]!="retired"{return Err(Error::bad("Use draft review to approve a memory"));}
                    let data=old["data"].clone();let result=memory::save(db,identifier,Some(s(old,"id")),args["expected_revision"].as_i64(),s(old,"kind"),s(old,"title"),s(old,"body"),"retired",&data)?;
                    db.execute("UPDATE am_claims SET state='retired',updated_at=? WHERE repository_id=? AND memory_id=?",params![now(),identifier,s(old,"id")])?;Ok(result)
                },
                "memory.delete"=>{
                    let memories=memory::list(db,&repository,p,true)?;let memory_id=string(args,"memory_id",100)?;
                    if !memories.iter().any(|m|s(m,"id")==memory_id){return Err(Error::bad("Memory unavailable"));}
                    // The memory, its dependents and everything naming them go in one transaction, with a hash-chained receipt.
                    let (deleted,counts)=crate::deletion::delete_memories(db,&p.workspace,identifier,&memories,memory_id)?;
                    purge_derived_cache(db,identifier)?;
                    let keys:Vec<String>=deleted.iter().cloned().collect();
                    let remaining=crate::deletion::memory_rows(db,identifier,&keys)?;
                    let receipt=crate::deletion::rows_receipt(db,p,identifier,"memory.delete","memories",json!({"rule":"memory_with_dependent_notes_and_historical_versions"}),counts,&keys,remaining,true)?;
                    Ok(json!({"deleted":deleted.len(),"transitive":true,"receipt":receipt}))
                },
                // An agent's proposal is always a draft citing exact quotes of
                // current sources; approval is a separate `learning.review`.
                "memory.propose"=>{
                    let kind=string(args,"kind",30)?;
                    if !["decision","preference","procedure","task_state","note"].contains(&kind){return Err(Error::bad("Choose decision, preference, procedure, task_state or note"));}
                    let title=string(args,"title",500)?;let body=string(args,"body",64000)?;let key=string(args,"idempotency_key",200)?;
                    let reason=args["reason"].as_str().unwrap_or("");if reason.len()>2000{return Err(Error::bad("Reason exceeds 2,000 bytes"));}
                    let files=args["files"].as_array().cloned().unwrap_or_default();if files.len()>50{return Err(Error::bad("Name at most 50 files"));}
                    for file in &files{store::path(file.as_str().ok_or_else(||Error::bad("Invalid path"))?)?;}
                    // Symbols the memory is about, by
                    // stable id or by path and qualified name.
                    let named=match args.get("symbols"){None|Some(Value::Null)=>Vec::new(),Some(Value::Array(items))=>items.clone(),Some(_)=>return Err(Error::bad("symbols must be a list"))};
                    if named.len()>crate::symbols::MAX_NAMED{return Err(Error::bad(format!("Name at most {} symbols",crate::symbols::MAX_NAMED)));}
                    let evidence=args["evidence"].as_array().filter(|v|!v.is_empty() && v.len()<=20).ok_or_else(||Error::bad("Cite 1 to 20 exact source quotes"))?;
                    let mut citations=Vec::new();
                    for item in evidence{
                        let quote=string(item,"quote",12000)?;if quote.trim().chars().count()<8{return Err(Error::bad("Each quote needs at least 8 characters"));}
                        citations.push(json!({"id":string(item,"source_id",1500)?,"quote":quote}));
                    }
                    let recall=args.get("recall").map(crate::structured_memory::metadata).transpose()?.map(serde_json::to_value).transpose()?;
                    let mut proposal=json!({"kind":kind,"title":title,"body":body,"reason":reason,"files":files,"evidence":citations});
                    if let Some(ref value)=recall { proposal["recall"]=value.clone(); }
                    // Only a proposal naming symbols carries them, so earlier
                    // proposals keep their fingerprint.
                    if !named.is_empty() { proposal["symbols"]=json!(named); }
                    let fingerprint=crate::digest(&proposal);
                    // Replays compare the whole proposal; a key names one proposal only.
                    if let Some(old)=one(db,"SELECT id,state,json_extract(data,'$.proposal.digest') AS digest FROM repo_memories WHERE repository_id=? AND json_extract(data,'$.proposal.idempotency_key')=?",params![identifier,key])?{
                        if old["digest"]!=json!(fingerprint){return Err(Error::conflict("Idempotency key already refers to a different proposal"));}
                        return Ok(json!({"memory_id":old["id"],"state":old["state"],"replayed":true}));
                    }
                    // Every quote must appear in a source the caller can read now.
                    let docs=memory::documents(db,&repository,p)?;
                    let mut data=memory::dependencies(db,identifier,&citations,&docs)?;
                    crate::symbols::name(db,identifier,&named,&docs,&mut data)?;
                    // The named files become dependencies,
                    // judged as whole files; an unknown path is refused.
                    let mut named_files=Vec::new();
                    for file in &files{
                        let path=file.as_str().unwrap_or("");
                        let doc=docs.iter().find(|d|s(d,"id")==format!("file:{path}")).ok_or_else(||Error::bad(format!("{path} is not in the project's current sources")))?;
                        data["dependencies"][path]=doc["sha256"].clone();
                        named_files.push(path.to_string());
                    }
                    crate::symbols::name_files(&mut data,&named_files);
                    data["authority"]=json!("agent_proposed_draft");data["notes"]=json!({});
                    if let Some(recall)=recall { data["recall"]=recall; }
                    data["proposal"]=json!({"idempotency_key":key,"digest":fingerprint,"reason":reason,"files":files,"proposed_at":now()});
                    let saved=memory::save(db,identifier,None,None,kind,title,body,"draft",&data)?;
                    store::event(db,identifier,"memory_proposed",json!({"id":saved["id"],"kind":kind}))?;
                    Ok(json!({"memory_id":saved["id"],"revision":saved["revision"],"state":"draft","approval_required":true,"review":"learning.review"}))
                },
                "memory.import"=>{
                    let values=args["memories"].as_array().filter(|v|v.len()<=500).ok_or_else(||Error::bad("Import at most 500 memories"))?;let mut imported=Vec::new();
                    // Symbol links are
                    // re-grounded here through their logical identity.
                    let docs=memory::documents(db,&repository,p)?;let mut regrounded=0;let mut unresolved=0;
                    for value in values {let mut data=value.get("data").cloned().unwrap_or(json!({}));if !data.is_object(){return Err(Error::bad("Invalid memory data"));}
                        if let Some(value)=data.get("recall") { data["recall"]=serde_json::to_value(crate::structured_memory::metadata(value)?)?; }
                        data["authority"]=json!("unreviewed_import");data["notes"]=json!({});data.as_object_mut().unwrap().remove("refresh_target");data.as_object_mut().unwrap().remove("target_revision");
                        for key in ["dependencies","evidence_links","native_sources"]{if !data[key].is_object(){data[key]=json!({});}}
                        if data.get("symbols").is_some_and(|v|!v.is_array()) || data.get("symbol_files").is_some_and(|v|!v.is_object()){return Err(Error::bad("Invalid symbol links"));}
                        let report=crate::symbols::reground(db,identifier,&mut data,&docs)?;
                        regrounded+=report["regrounded"].as_u64().unwrap_or(0);unresolved+=report["unresolved"].as_u64().unwrap_or(0);
                        imported.push(memory::save(db,identifier,None,None,string(value,"kind",100)?,string(value,"title",500)?,string(value,"body",64000)?,"draft",&data)?);
                    }
                    let mut result=json!({"imported":imported,"approval_required":true});
                    // Only an import that carried symbol links reports them.
                    if regrounded+unresolved>0{result["symbol_links"]=json!({"regrounded":regrounded,"unresolved":unresolved});}
                    Ok(result)
                },
                _=>Err(Error::bad("Unsupported project mutation"))
            }
        })?;
        if result.get("receipt").is_some() {
            // After commit, flush the deleted pages' old images out of the WAL.
            result["storage"] = crate::deletion::checkpoint(self)?;
        }
        Ok(result)
    }
}
pub fn purge_derived_cache(db: &rusqlite::Connection, repository: &str) -> Result<()> {
    for table in [
        "am_model_cache",
        "am_query_cache",
        "am_vectors",
        "am_document_reads",
        "am_preparation_steps",
        "am_reports",
        "am_communities",
    ] {
        db.execute(
            &format!("DELETE FROM {table} WHERE repository_id=?"),
            [repository],
        )?;
    }
    db.execute("DELETE FROM am_jobs WHERE repository_id=?", [repository])?;
    store::invalidate(db, repository)
}
pub fn redact(value: &str) -> String {
    use std::sync::OnceLock;
    static SECRET: OnceLock<regex::Regex> = OnceLock::new();
    SECRET
        .get_or_init(|| {
            regex::Regex::new(
                r#"(?i)(?:bearer\s+|(?:api[_-]?key|token|password|secret)\s*[=:]\s*)[^\s'\"]+"#,
            )
            .unwrap()
        })
        .replace_all(value, "[credential redacted]")
        .to_string()
}
