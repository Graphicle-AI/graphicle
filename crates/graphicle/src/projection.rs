//! Durable removal of only this application's external graph/vector scopes.
//! Requests survive project removal, process failure and credential outages.
use crate::{
    Error, Result, digest, id, now,
    store::{Store, one, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

fn address_hash(config: &Value) -> String {
    let mut address = config.clone();
    if let Some(map) = address.as_object_mut() {
        map.remove("credential_ref");
    }
    digest(&address)
}

pub fn enqueue(db: &Connection, repository: &str, reason: &str) -> Result<()> {
    let Some(project) = one(
        db,
        "SELECT workspace_id FROM repo_projects WHERE id=?",
        [repository],
    )?
    else {
        return Ok(());
    };
    for backend in rows(
        db,
        "SELECT kind,config FROM am_backends WHERE repository_id=?",
        [repository],
    )? {
        if !["neo4j", "qdrant"].contains(&s(&backend, "kind")) {
            continue;
        }
        let config: Value = serde_json::from_str(s(&backend, "config"))?;
        enqueue_config(
            db,
            s(&project, "workspace_id"),
            repository,
            s(&backend, "kind"),
            &config,
            reason,
        )?;
    }
    Ok(())
}
pub fn enqueue_config(
    db: &Connection,
    workspace: &str,
    scope: &str,
    kind: &str,
    config: &Value,
    reason: &str,
) -> Result<()> {
    if !["neo4j", "qdrant"].contains(&kind) {
        return Err(Error::bad("Unsupported projection cleanup"));
    }
    let stamp = now();
    db.execute("INSERT INTO am_projection_cleanup(id,workspace_id,scope,kind,config,config_hash,revision,state,not_before,reason,created_at,updated_at) VALUES(?,?,?,?,?,?,1,'pending',?,?,?,?) ON CONFLICT(scope,kind,config_hash) DO UPDATE SET config=excluded.config,revision=revision+1,state=CASE WHEN state='running' THEN 'running' ELSE 'pending' END,not_before=excluded.not_before,reason=excluded.reason,updated_at=excluded.updated_at",params![id(),workspace,scope,kind,config.to_string(),address_hash(config),stamp,reason,stamp,stamp])?;
    Ok(())
}
pub fn ready(db: &Connection, repository: &str, kind: &str, config: &Value) -> Result<()> {
    if one(db,"SELECT id FROM am_projection_cleanup WHERE scope=? AND kind=? AND config_hash=? AND state!='complete'",params![repository,kind,address_hash(config)])?.is_some(){
        return Err(Error::conflict("External cleanup is pending. Run the worker or retry cleanup before publishing this projection"));
    }
    Ok(())
}
/// Changes even when cleanup finishes during an in-flight publication.
pub fn marker(db: &Connection, scope: &str, kind: &str, config: &Value) -> Result<i64> {
    Ok(one(
        db,
        "SELECT revision FROM am_projection_cleanup WHERE scope=? AND kind=? AND config_hash=?",
        params![scope, kind, address_hash(config)],
    )?
    .and_then(|v| v["revision"].as_i64())
    .unwrap_or(0))
}
pub fn status(store: &Store, workspace: &str, token: &str) -> Result<Value> {
    store.authorized(workspace,token,"admin",|db,_|Ok(json!({"cleanups":rows(db,"SELECT id,scope,kind,state,attempts,reason,error,created_at,updated_at FROM am_projection_cleanup WHERE workspace_id=? ORDER BY updated_at DESC LIMIT 200",[workspace])?})))
}
pub fn retry(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    if let Some(reference) = args["credential_ref"].as_str() {
        store.resolve_credential(workspace, reference)?;
    }
    store.authorized(workspace,token,"admin",|db,_|{
        let identifier=crate::string(args,"cleanup_id",100)?;
        let row=one(db,"SELECT * FROM am_projection_cleanup WHERE workspace_id=? AND id=?",params![workspace,identifier])?.ok_or_else(||Error::bad("Cleanup unavailable"))?;
        if row["state"]=="running"{return Err(Error::conflict("Cleanup is already running"));}
        let mut cfg:Value=serde_json::from_str(s(&row,"config"))?;
        if let Some(reference)=args["credential_ref"].as_str(){cfg["credential_ref"]=json!(reference);}
        db.execute("UPDATE am_projection_cleanup SET config=?,state='pending',error=NULL,not_before=?,updated_at=? WHERE id=?",params![cfg.to_string(),now(),now(),identifier])?;
        Ok(json!({"queued":true,"id":identifier}))
    })
}
pub async fn once(store: &Store, client: &reqwest::Client) -> Result<bool> {
    let leased=store.transaction(|db|{
        db.execute("UPDATE am_projection_cleanup SET state='pending',lease_token=NULL WHERE state='running' AND lease_until<?",[now()])?;
        let Some(mut row)=one(db,"SELECT * FROM am_projection_cleanup WHERE state='pending' AND not_before<=? ORDER BY updated_at LIMIT 1",[now()])? else{return Ok(None)};
        let lease=id();let until=(chrono::Utc::now()+chrono::Duration::seconds(300)).to_rfc3339_opts(chrono::SecondsFormat::Micros,true);
        db.execute("UPDATE am_projection_cleanup SET state='running',attempts=attempts+1,lease_token=?,lease_until=?,updated_at=? WHERE id=?",params![lease,until,now(),s(&row,"id")])?;
        row["lease_token"]=json!(lease);Ok(Some(row))
    })?;
    let Some(row) = leased else { return Ok(false) };
    let config: Value = serde_json::from_str(s(&row, "config"))?;
    let result = match s(&row, "kind") {
        "neo4j" => {
            crate::graph::clear_projection(
                store,
                client,
                s(&row, "workspace_id"),
                &config,
                s(&row, "scope"),
            )
            .await
        }
        "qdrant" => {
            crate::vector::clear_projection(
                store,
                client,
                s(&row, "workspace_id"),
                &config,
                s(&row, "scope"),
            )
            .await
        }
        _ => Err(Error::bad("Unknown external projection")),
    };
    store.transaction(|db|{
        let current=one(db,"SELECT * FROM am_projection_cleanup WHERE id=? AND lease_token=?",params![s(&row,"id"),s(&row,"lease_token")])?.ok_or_else(||Error::conflict("Cleanup lease changed"))?;
        let again=current["revision"]!=row["revision"];
        let state=if result.is_ok() && !again {"complete"}else{"pending"};
        let delay=if result.is_ok(){0}else{30i64.saturating_mul(1i64 << current["attempts"].as_u64().unwrap_or(1).min(7)).min(3600)};
        let later=(chrono::Utc::now()+chrono::Duration::seconds(delay)).to_rfc3339_opts(chrono::SecondsFormat::Micros,true);
        db.execute("UPDATE am_projection_cleanup SET state=?,lease_token=NULL,lease_until=NULL,error=?,not_before=?,updated_at=? WHERE id=? AND lease_token=?",params![state,result.as_ref().err().map(|e|e.message.clone()),later,now(),s(&row,"id"),s(&row,"lease_token")])?;
        Ok(())
    })?;
    Ok(true)
}
