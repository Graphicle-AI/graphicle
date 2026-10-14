//! Optional Qdrant index. Only identities and vectors are projected; original text stays canonical.
use crate::{
    Error, Result,
    jobs::{self, Job, Worker},
    model, query,
    store::{Store, decode, one, repo, rows, s},
    string,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn point(identity: &str) -> String {
    let h = graphicle_memory::text::sha256(identity.as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
pub fn configure(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, token, "admin", |db, p| {
        repo(db, p, string(args, "repository_id", 100)?)?;
        Ok(())
    })?;
    let endpoint = string(args, "endpoint", 2000)?;
    model::checked_url(endpoint)?;
    let reference = args["credential_ref"].as_str();
    if let Some(r) = reference {
        store.resolve_credential(workspace, r)?;
    }
    let collection = string(args, "collection", 100)?;
    if !collection
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::bad("Use an alphanumeric collection name"));
    }
    let config = json!({"endpoint":endpoint,"collection":collection,"credential_ref":reference});
    store.authorized(workspace,token,"admin",|db,p|{let repository=repo(db,p,string(args,"repository_id",100)?)?;if let Ok(old)=self::config(db,s(&repository,"id"))&& old!=config{crate::projection::enqueue_config(db,workspace,s(&repository,"id"),"qdrant",&old,"backend_reconfigured")?;}db.execute("INSERT OR REPLACE INTO am_backends VALUES(?,?,?)",params![s(&repository,"id"),"qdrant",config.to_string()])?;Ok(json!({"configured":true,"stored_remotely":"vectors and opaque source identities; no original text"}))})
}
async fn request(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut request = client.request(
        method,
        format!(
            "{}/collections/{}{}",
            s(config, "endpoint").trim_end_matches('/'),
            s(config, "collection"),
            path
        ),
    );
    if let Some(reference) = config["credential_ref"].as_str() {
        request = request.header("api-key", store.resolve_credential(workspace, reference)?);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| Error::bad("Vector index request failed"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(json!({"missing":true}));
    }
    model::bounded_json(response).await
}
fn config(db: &rusqlite::Connection, repository: &str) -> Result<Value> {
    let mut row = one(
        db,
        "SELECT config FROM am_backends WHERE repository_id=? AND kind='qdrant'",
        [repository],
    )?
    .ok_or_else(|| Error::bad("Configure a Qdrant collection first"))?;
    decode(&mut row, "config")?;
    Ok(row["config"].clone())
}
pub(crate) async fn clear_projection(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    scope: &str,
) -> Result<()> {
    request(
        store,
        client,
        workspace,
        config,
        reqwest::Method::POST,
        "/points/delete?wait=true",
        Some(json!({"filter":{"must":[{"key":"scope","match":{"value":scope}}]}})),
    )
    .await?;
    Ok(())
}
pub async fn publish(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    token: &str,
    args: &Value,
) -> Result<Value> {
    let (config, snapshot, vectors, revision) =
        store.authorized(workspace, token, "admin", |db, p| {
            let r = repo(db, p, string(args, "repository_id", 100)?)?;
            let snapshot = query::snapshot(db, &r, p)?;
            let (candidates, _) = query::candidates(&snapshot)?;
            let valid = candidates
                .iter()
                .map(|c| (s(c, "identity"), s(c, "source_sha")))
                .collect::<BTreeMap<_, _>>();
            let mut vectors = rows(
                db,
                "SELECT * FROM am_vectors WHERE repository_id=? AND model_key=? ORDER BY identity",
                params![s(&r, "id"), string(args, "model_key", 64)?],
            )?;
            vectors.retain(|v| valid.get(s(v, "identity")) == Some(&s(v, "source_sha")));
            for v in &mut vectors {
                decode(v, "vector")?;
            }
            let cfg = config(db, s(&r, "id"))?;
            crate::projection::ready(db, s(&r, "id"), "qdrant", &cfg)?;
            let revision = crate::projection::marker(db, s(&r, "id"), "qdrant", &cfg)?;
            Ok((cfg, snapshot, vectors, revision))
        })?;
    if vectors.is_empty() {
        return Err(Error::bad(
            "Embed this source snapshot before publishing its vector index",
        ));
    }
    let size = vectors[0]["dimensions"].as_u64().unwrap_or(0);
    if size == 0 || vectors.iter().any(|v| v["dimensions"] != size) {
        return Err(Error::bad("Vector dimensions do not match"));
    }
    let scope = s(&snapshot.repository, "id");
    let mut touched = false;
    let result=async {
    let exists = request(
        store,
        client,
        workspace,
        &config,
        reqwest::Method::GET,
        "",
        None,
    )
    .await?;
    if exists["missing"] == true {
        touched=true;
        request(
            store,
            client,
            workspace,
            &config,
            reqwest::Method::PUT,
            "",
            Some(json!({"vectors":{"size":size,"distance":"Cosine"}})),
        )
        .await?;
    } else if exists["result"]["config"]["params"]["vectors"]["size"] != size {
        return Err(Error::bad(
            "Qdrant collection dimensions differ from the embedding model",
        ));
    }
    let generation = crate::id();
    let model_key = string(args, "model_key", 64)?;
    for batch in vectors.chunks(64) {
        store.authorized(workspace, token, "admin", |db, p| {
            let r = repo(db, p, scope)?;
            if query::snapshot(db, &r, p)?.state != snapshot.state || crate::projection::marker(db, scope, "qdrant", &config)? != revision {
                return Err(Error::conflict("Sources changed during vector publication"));
            }
            Ok(())
        })?;
        let points=batch.iter().map(|v|json!({"id":point(&format!("{scope}:{model_key}:{}",s(v,"identity"))),"vector":v["vector"],"payload":{"scope":scope,"identity":v["identity"],"source_sha":v["source_sha"],"model_key":model_key,"generation":generation}})).collect::<Vec<_>>();
        touched=true;
        request(
            store,
            client,
            workspace,
            &config,
            reqwest::Method::PUT,
            "/points?wait=true",
            Some(json!({"points":points})),
        )
        .await?;
    }
    let current = store.authorized(workspace, token, "admin", |db, p| {
        let r = repo(db, p, scope)?;
        Ok(query::snapshot(db, &r, p)?.state == snapshot.state && crate::projection::marker(db, scope, "qdrant", &config)? == revision)
    })?;
    if !current {
        return Err(Error::conflict(
            "Source snapshot changed; only current identities can be retrieved, and publication should be repeated",
        ));
    }
    request(store,client,workspace,&config,reqwest::Method::POST,"/points/delete?wait=true",Some(json!({"filter":{"must":[{"key":"scope","match":{"value":scope}},{"key":"model_key","match":{"value":model_key}}],"must_not":[{"key":"generation","match":{"value":generation}}]}}))).await?;
    Ok(
        json!({"published":vectors.len(),"dimensions":size,"model_key":model_key,"source_state":snapshot.state}),
    )
    }.await;
    if touched && result.is_err() {
        store.transaction(|db| {
            crate::projection::enqueue_config(
                db,
                workspace,
                scope,
                "qdrant",
                &config,
                "publication_incomplete",
            )
        })?;
    }
    result
}
pub async fn ranking(
    worker: &Worker,
    job: &Job,
    candidates: &[Value],
    vector: &[f64],
    model_key: &str,
) -> Result<Value> {
    if vector.is_empty() || vector.len() > 8192 || vector.iter().any(|n| !n.is_finite()) {
        return Err(Error::bad("Invalid search vector"));
    }
    let config = jobs::guarded_read(&worker.store, job, |db, _, _| config(db, &job.repository))?;
    let valid = candidates
        .iter()
        .map(|v| (s(v, "identity"), s(v, "source_sha")))
        .collect::<BTreeMap<_, _>>();
    let ids = candidates
        .iter()
        .map(|c| {
            point(&format!(
                "{}:{model_key}:{}",
                job.repository,
                s(c, "identity")
            ))
        })
        .collect::<BTreeSet<_>>();
    // Pre-filter on canonical IDs and check the source fingerprint again below.
    let result=request(&worker.store,&worker.client,&job.workspace,&config,reqwest::Method::POST,"/points/query",Some(json!({"query":vector,"limit":200,"with_payload":true,"filter":{"must":[{"has_id":ids},{"key":"scope","match":{"value":job.repository}},{"key":"model_key","match":{"value":model_key}}]}}))).await?;
    if result["missing"] == true {
        return Err(Error::bad(
            "Qdrant collection is unavailable; publish the index first",
        ));
    }
    jobs::guarded_read(&worker.store, job, |_, _, _| Ok(()))?;
    let ranked = result["result"]["points"]
        .as_array()
        .ok_or_else(|| Error::bad("Vector backend returned an invalid result"))?
        .iter()
        .filter(|p| {
            p["payload"]["scope"] == job.repository
                && p["payload"]["model_key"] == model_key
                && valid.get(s(&p["payload"], "identity")) == Some(&s(&p["payload"], "source_sha"))
        })
        .map(|p| p["payload"]["identity"].clone())
        .collect::<Vec<_>>();
    Ok(json!({"semantic":ranked}))
}
