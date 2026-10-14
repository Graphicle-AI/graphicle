//! Redis/Valkey inference dispatch. The coordinator alone owns sources, permissions and commits.
//! Consumers receive bounded inference requests, never workspace capabilities or provider secrets.
use crate::{
    Error, Result, digest, id,
    jobs::{self, Job, Worker},
    model,
    store::{Store, decode, one, repo, s},
    string,
};
use redis::{
    AsyncCommands,
    streams::{StreamAutoClaimReply, StreamReadReply},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    pub endpoint: String,
    pub namespace: String,
    #[serde(default)]
    pub username: Option<String>,
    pub credential_ref: String,
}
fn validate(queue: &Queue) -> Result<url::Url> {
    let url = url::Url::parse(&queue.endpoint).map_err(|_| Error::bad("Invalid queue endpoint"))?;
    let local = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if !(url.scheme() == "rediss" || (url.scheme() == "redis" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::bad(
            "Use a TLS Redis endpoint, or Redis on loopback, with credentials stored separately",
        ));
    }
    if queue.namespace.is_empty()
        || queue.namespace.len() > 80
        || !queue
            .namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::bad("Use an alphanumeric queue namespace"));
    }
    Ok(url)
}
async fn connection(queue: &Queue, password: &str) -> Result<redis::aio::MultiplexedConnection> {
    let mut url = validate(queue)?;
    url.set_username(queue.username.as_deref().unwrap_or("default"))
        .map_err(|_| Error::bad("Invalid queue username"))?;
    url.set_password(Some(password))
        .map_err(|_| Error::bad("Invalid queue credential"))?;
    let client =
        redis::Client::open(url.as_str()).map_err(|_| Error::bad("Invalid queue configuration"))?;
    tokio::time::timeout(
        Duration::from_secs(10),
        client.get_multiplexed_async_connection(),
    )
    .await
    .map_err(|_| Error::bad("Queue connection timed out"))?
    .map_err(|_| Error::bad("Queue authentication or connection failed"))
}
fn queue_error(_: redis::RedisError) -> Error {
    Error::bad("Queue operation failed")
}
pub fn configure(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, token, "admin", |db, p| {
        repo(db, p, string(args, "repository_id", 100)?)?;
        Ok(())
    })?;
    let queue: Queue = serde_json::from_value(args["config"].clone())?;
    validate(&queue)?;
    store.resolve_credential(workspace, &queue.credential_ref)?;
    store.authorized(workspace,token,"admin",|db,p|{let repository=repo(db,p,string(args,"repository_id",100)?)?;db.execute("INSERT OR REPLACE INTO am_backends VALUES(?,?,?)",params![s(&repository,"id"),"redis",serde_json::to_string(&queue)?])?;Ok(json!({"configured":true,"dispatch":"explicit per job","source_authority":"coordinator"}))})
}
#[derive(Serialize, Deserialize)]
struct Request {
    version: u32,
    id: String,
    request_hash: String,
    model_key: String,
    model: String,
    instructions: String,
    input: Value,
    schema: Value,
    output_limit: usize,
    expires_at: i64,
}
pub async fn generate(
    worker: &Worker,
    job: &Job,
    profile: &model::Profile,
    instructions: &str,
    input: &Value,
    schema: &Value,
    output_limit: usize,
) -> Result<model::Reply> {
    let queue: Queue = jobs::guarded(&worker.store, job, |db, _, _| {
        let mut row = one(
            db,
            "SELECT config FROM am_backends WHERE repository_id=? AND kind='redis'",
            [&job.repository],
        )?
        .ok_or_else(|| Error::bad("Configure a distributed queue for this project"))?;
        decode(&mut row, "config")?;
        Ok(serde_json::from_value(row["config"].clone())?)
    })?;
    let password = worker
        .store
        .resolve_credential(&job.workspace, &queue.credential_ref)?;
    let mut conn = connection(&queue, &password).await?;
    let identifier = id();
    let request_hash = digest(&json!([
        model::key(profile),
        instructions,
        input,
        schema,
        output_limit
    ]));
    let request = Request {
        version: 1,
        id: identifier.clone(),
        request_hash: request_hash.clone(),
        model_key: model::key(profile),
        model: profile.model.clone(),
        instructions: instructions.into(),
        input: input.clone(),
        schema: schema.clone(),
        output_limit,
        expires_at: chrono::Utc::now().timestamp() + 220,
    };
    let body = serde_json::to_string(&request)?;
    if body.len() > 1024 * 1024 {
        return Err(Error::bad("Distributed inference request exceeds 1 MiB"));
    }
    let stream = format!("{}:requests", queue.namespace);
    let task_key = format!("{}:request:{identifier}", queue.namespace);
    let result_key = format!("{}:result:{identifier}", queue.namespace);
    let length: usize = conn.xlen(&stream).await.map_err(queue_error)?;
    if length >= 1000 {
        return Err(Error::bad("Distributed queue is full"));
    }
    let _: () = redis::pipe()
        .atomic()
        .cmd("SET")
        .arg(&task_key)
        .arg(&body)
        .arg("EX")
        .arg(240)
        .ignore()
        .cmd("XADD")
        .arg(&stream)
        .arg("*")
        .arg("request")
        .arg(&identifier)
        .ignore()
        .query_async(&mut conn)
        .await
        .map_err(queue_error)?;
    let start = Instant::now();
    loop {
        if let Err(error) = jobs::guarded(&worker.store, job, |db, _, _| jobs::renew(db, job)) {
            let _: std::result::Result<usize, _> = conn.del(&task_key).await;
            return Err(error);
        }
        let result: Option<String> = conn.get_del(&result_key).await.map_err(queue_error)?;
        if let Some(result) = result {
            if result.len() > 4 * 1024 * 1024 {
                return Err(Error::bad("Distributed result exceeds limits"));
            }
            let value: Value = serde_json::from_str(&result)?;
            if value["request_id"] != identifier || value["request_hash"] != request_hash {
                return Err(Error::bad("Distributed result identity mismatch"));
            }
            if let Some(error) = value["error"].as_str() {
                return Err(Error::bad(error));
            }
            return serde_json::from_value(value["reply"].clone()).map_err(Into::into);
        }
        if start.elapsed() > Duration::from_secs(225) {
            let _: std::result::Result<usize, _> = conn.del(&task_key).await;
            return Err(Error::bad(
                "Distributed inference timed out; provider usage may be unknown",
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn consume(
    queue: &Queue,
    password: &str,
    profile: &model::Profile,
    credential: Option<&str>,
    once: bool,
) -> Result<Value> {
    let mut conn = connection(queue, password).await?;
    let stream = format!("{}:requests", queue.namespace);
    // Frozen consumer-group name: a renamed group would be created at "0" and
    // re-deliver every entry still in the stream to the workers.
    let group = "graphicle-inference-v1";
    let consumer = id();
    let create: std::result::Result<(), _> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(&stream)
        .arg(group)
        .arg("0")
        .arg("MKSTREAM")
        .query_async(&mut conn)
        .await;
    if let Err(error) = create
        && error.code() != Some("BUSYGROUP")
    {
        return Err(queue_error(error));
    }
    let client = model::client()?;
    let mut processed = 0usize;
    loop {
        // Requests expire after 220 seconds. Reap abandoned deliveries after five
        // minutes without repeating a possibly billed model call.
        let abandoned: StreamAutoClaimReply = redis::cmd("XAUTOCLAIM")
            .arg(&stream)
            .arg(group)
            .arg(&consumer)
            .arg(300000)
            .arg("0-0")
            .arg("COUNT")
            .arg(100)
            .query_async(&mut conn)
            .await
            .map_err(queue_error)?;
        for entry in abandoned.claimed {
            let _: () = redis::pipe()
                .atomic()
                .cmd("XACK")
                .arg(&stream)
                .arg(group)
                .arg(&entry.id)
                .ignore()
                .cmd("XDEL")
                .arg(&stream)
                .arg(&entry.id)
                .ignore()
                .query_async(&mut conn)
                .await
                .map_err(queue_error)?;
        }
        let read: StreamReadReply = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(group)
            .arg(&consumer)
            .arg("COUNT")
            .arg(1)
            .arg("BLOCK")
            .arg(1000)
            .arg("STREAMS")
            .arg(&stream)
            .arg(">")
            .query_async(&mut conn)
            .await
            .map_err(queue_error)?;
        if read.keys.is_empty() {
            if once {
                return Ok(json!({"processed":processed}));
            }
            continue;
        }
        for stream_item in read.keys {
            for item in stream_item.ids {
                let identifier = item
                    .map
                    .get("request")
                    .and_then(|v| redis::from_redis_value::<String>(v).ok())
                    .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| Error::bad("Malformed queue request identity"))?;
                let task_key = format!("{}:request:{identifier}", queue.namespace);
                let result_key = format!("{}:result:{identifier}", queue.namespace);
                let lock_key = format!("{}:claim:{identifier}", queue.namespace);
                let body: Option<String> = conn.get(&task_key).await.map_err(queue_error)?;
                let claim: Option<String> = redis::cmd("SET")
                    .arg(&lock_key)
                    .arg(&consumer)
                    .arg("NX")
                    .arg("EX")
                    .arg(300)
                    .query_async(&mut conn)
                    .await
                    .map_err(queue_error)?;
                if let Some(body) = body
                    && claim.is_some()
                {
                    let parsed = if body.len() <= 1024 * 1024 {
                        serde_json::from_str::<Request>(&body).ok()
                    } else {
                        None
                    };
                    if let Some(request) = parsed {
                        let computed = digest(&json!([
                            request.model_key,
                            request.instructions,
                            request.input,
                            request.schema,
                            request.output_limit
                        ]));
                        let valid = request.version == 1
                            && request.id == identifier
                            && request.request_hash == computed
                            && request.model_key == model::key(profile)
                            && request.model == profile.model
                            && request.expires_at > chrono::Utc::now().timestamp();
                        let result = if valid {
                            let remaining =
                                (request.expires_at - chrono::Utc::now().timestamp()).max(1) as u64;
                            tokio::time::timeout(
                                Duration::from_secs(remaining.min(180)),
                                model::generate_with_credential(
                                    &client,
                                    profile,
                                    &request.instructions,
                                    &request.input,
                                    &request.schema,
                                    request.output_limit,
                                    credential,
                                ),
                            )
                            .await
                            .unwrap_or_else(|_| Err(Error::bad("Worker model request timed out")))
                        } else {
                            Err(Error::bad(
                                "Worker rejected an expired, altered or mismatched model request",
                            ))
                        };
                        let reply = match result {
                            Ok(reply) => {
                                json!({"request_id":identifier,"request_hash":request.request_hash,"reply":reply})
                            }
                            Err(error) => {
                                json!({"request_id":identifier,"request_hash":request.request_hash,"error":error.message})
                            }
                        };
                        let exists: bool = conn.exists(&task_key).await.map_err(queue_error)?;
                        if exists {
                            let _: () = conn
                                .set_ex(&result_key, reply.to_string(), 300)
                                .await
                                .map_err(queue_error)?;
                        }
                        processed += 1;
                    }
                }
                let _: () = redis::pipe()
                    .atomic()
                    .cmd("XACK")
                    .arg(&stream)
                    .arg(group)
                    .arg(&item.id)
                    .ignore()
                    .cmd("XDEL")
                    .arg(&stream)
                    .arg(&item.id)
                    .ignore()
                    .query_async(&mut conn)
                    .await
                    .map_err(queue_error)?;
                if claim.is_some() {
                    let _: usize = conn
                        .del(&[&task_key, &lock_key])
                        .await
                        .map_err(queue_error)?;
                }
            }
        }
        if once {
            return Ok(json!({"processed":processed}));
        }
    }
}
