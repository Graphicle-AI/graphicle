//! Leased, bounded jobs. Network calls never hold a SQLite transaction.
use crate::{Error, Result, digest, id, integer, now, string};
use crate::{
    memory, model,
    store::{self, Principal, Store, authorize_hash, decode, one, repo, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};

// All work is serialized by repository unless both jobs explicitly opt into
// independent observer sources. Prefix intersection is conservative: even a
// newly ingested source cannot belong to both groups. Compare bytes, matching
// Rust's starts_with (including non-ASCII and embedded-NUL prefixes).
const RUNNING_CONFLICT: &str = r#"
EXISTS (
    SELECT 1 FROM am_jobs r
    WHERE r.repository_id=j.repository_id AND r.state='running'
    AND NOT coalesce(
        j.kind='observer_notes' AND r.kind='observer_notes'
        AND json_type(j.payload,'$.parallel_sources')='true'
        AND json_type(r.payload,'$.parallel_sources')='true'
        AND json_type(j.payload,'$.include')='array'
        AND json_type(r.payload,'$.include')='array'
        AND json_array_length(j.payload,'$.include')>0
        AND json_array_length(r.payload,'$.include')>0
        AND NOT EXISTS (
            SELECT 1 FROM json_each(j.payload,'$.include') a
            CROSS JOIN json_each(r.payload,'$.include') b
            WHERE substr(CAST(a.value AS BLOB),1,length(CAST(b.value AS BLOB)))=CAST(b.value AS BLOB)
               OR substr(CAST(b.value AS BLOB),1,length(CAST(a.value AS BLOB)))=CAST(a.value AS BLOB)
        ), 0
    )
)"#;

static READY_JOB_QUERY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT kind FROM am_jobs j WHERE j.state IN ('queued','retry') AND j.not_before<=?1 AND NOT {RUNNING_CONFLICT} ORDER BY j.priority DESC,j.created_at LIMIT 1"
    )
});

// ?5 is false when this program cannot sync provider permissions: then the
// sync of a connector in provider permission mode is left queued (as are
// `permissions` jobs, through the excluded kinds) for a program that can.
// ?6 lists the job kinds this program knows: a job of any other kind (queued
// by an extension this program lacks) is left queued for a program that runs
// it, and never hides known work behind the candidate cap.
static LEASE_JOB_QUERY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT j.*,p.workspace_id FROM am_jobs j JOIN repo_projects p ON p.id=j.repository_id WHERE j.state IN ('queued','retry') AND j.not_before<=?1 AND j.kind NOT IN (SELECT value FROM json_each(?2)) AND j.kind IN (SELECT value FROM json_each(?6)) AND (?3 IS NULL OR (j.repository_id=?3 AND j.kind=?4)) AND (?5 OR j.kind!='connector' OR NOT EXISTS (SELECT 1 FROM am_connectors c WHERE c.repository_id=j.repository_id AND c.id=json_extract(j.payload,'$.connector_id') AND json_extract(c.config,'$.permission_mode')='provider')) AND NOT {RUNNING_CONFLICT} ORDER BY j.priority DESC,j.created_at LIMIT 100"
    )
});

/// Digest of the code job results depend on, recorded in job payloads (a
/// queued workflow's replay key includes it). Only files of this crate and
/// the memory engine are hashed, never an extension's, so every edition
/// computes the same revision.
pub fn runtime_revision() -> &'static str {
    static REVISION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    REVISION.get_or_init(||digest(&json!({"version":env!("CARGO_PKG_VERSION"),"workflows":include_str!("workflows.rs"),"observer":include_str!("observer.rs"),"search":include_str!("search.rs"),"query":include_str!("query.rs"),"workflow_retrieval":include_str!("workflow_retrieval.rs"),"evidence":include_str!("evidence.rs"),"memory_evidence":include_str!("memory_evidence.rs"),"memory":include_str!("memory.rs"),"graph":include_str!("graph.rs"),"models":include_str!("model.rs"),"claims":include_str!("claims.rs"),"answer_review":include_str!("answer_review.rs"),"connectors":include_str!("connectors.rs"),"atlassian":include_str!("atlassian.rs"),"projection":include_str!("projection.rs"),"permissions":include_str!("permissions.rs"),"semantic_contract":include_str!("../../graphicle-memory/src/semantic.rs"),"retrieval":include_str!("../../graphicle-memory/src/retrieval.rs"),"packing":include_str!("../../graphicle-memory/src/packing.rs"),"ingest":include_str!("../../graphicle-memory/src/ingest.rs"),"text":include_str!("../../graphicle-memory/src/text.rs"),"core":graphicle_memory::structure::REVISION})))
}

#[derive(Clone)]
pub struct Job {
    pub id: String,
    pub repository: String,
    pub workspace: String,
    pub principal_hash: String,
    pub lease: String,
    pub kind: String,
    pub payload: Value,
}
/// Source work (connector syncs, permission refreshes and probes, and
/// extension kinds declared as source work): no model call, no pinned source
/// state.
pub fn source_task(kind: &str) -> bool {
    ["connector", "permissions", "connector_probe"].contains(&kind)
        || crate::extension::installed()
            .job_kind(kind)
            .is_some_and(|declared| declared.source)
}
/// The job kind of a provider-permission refresh. The core knows its name
/// (permission, job status) but leaves running it to an extension that
/// syncs provider permissions.
pub const PERMISSIONS: &str = "permissions";
/// What a job of a kind this program does not know waits for.
pub const UNKNOWN_KIND: &str = "Waiting for a Graphicle edition that runs this kind of job";
/// Why this program can never run a job of `kind` for `payload`'s connector:
/// an unsupported kind (see [`crate::archived`]), a kind it does not know
/// (queued by an edition with more extensions), or provider-permission work,
/// which needs an extension that syncs provider permissions. Such a job is
/// never leased here, so a program that can run it still finds it queued.
pub fn unavailable(
    db: &Connection,
    repository: &str,
    kind: &str,
    payload: &Value,
) -> Result<Option<String>> {
    if let Some(archived) = crate::archived::kind(kind) {
        return Ok(Some(archived.waiting()));
    }
    if permission(kind).is_err() {
        return Ok(Some(UNKNOWN_KIND.to_string()));
    }
    if crate::connectors::provider_permission_sync() {
        return Ok(None);
    }
    let provider = match kind {
        PERMISSIONS => true,
        "connector" => one(
            db,
            "SELECT 1 AS provider FROM am_connectors WHERE repository_id=? AND id=? AND json_extract(config,'$.permission_mode')='provider'",
            params![repository, s(payload, "connector_id")],
        )?
        .is_some(),
        _ => false,
    };
    Ok(provider.then(|| crate::connectors::PROVIDER_SYNC_UNAVAILABLE.to_string()))
}
/// Every job kind the core knows, with the permission its requester needs.
/// `permissions` is known by name; an extension that syncs provider
/// permissions runs it.
const KINDS: &[(&str, &str)] = &[
    ("connector", "admin"),
    (PERMISSIONS, "admin"),
    ("connector_probe", "admin"),
    ("reflect", "run"),
    ("embed", "run"),
    ("recall", "run"),
    ("basic", "run"),
    ("extract", "propose"),
    ("refresh", "propose"),
    ("project", "read"),
    ("link", "read"),
    (crate::observer::KIND, "propose"),
];
/// Whether `kind` is a job kind of the core.
pub fn core_kind(kind: &str) -> bool {
    KINDS.iter().any(|(name, _)| *name == kind)
}
/// The permission a job of `kind` needs: the core's kinds, then the job
/// kinds installed extensions declare. An unsupported kind has none: it is
/// refused with its archive folder.
pub fn permission(kind: &str) -> Result<&'static str> {
    if let Some(archived) = crate::archived::kind(kind) {
        return Err(Error::bad(archived.message()));
    }
    KINDS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, permission)| *permission)
        .or_else(|| {
            crate::extension::installed()
                .job_kind(kind)
                .map(|declared| declared.permission)
        })
        .ok_or_else(|| Error::bad("Unknown workflow"))
}
/// Every job kind this program knows: the core's and the installed
/// extensions' declared kinds, never an unsupported one.
fn known_kinds() -> Vec<&'static str> {
    KINDS
        .iter()
        .map(|(name, _)| *name)
        .chain(crate::extension::installed().job_kinds())
        .filter(|kind| crate::archived::kind(kind).is_none())
        .collect()
}

/// The `input_hash` of the one `project` job per head-set episode.
pub fn projection_input_hash(episode: &str) -> String {
    digest(&json!({"kind":"project","episode":episode}))
}

/// The state of the projection of `episode`, a head set of `repository`,
/// over every `project` job queued for it: the sync's own job and any retry
/// of it, which has an input hash of its own. `running`
/// while one runs, else `queued` while one waits (queued or retry), else
/// `failed` when the newest one failed, else `none`: no job, a job that
/// finished without recording the head set (the heads had moved on), or a
/// cancelled one. Nothing will then project it until the next sync.
pub fn projection_state(db: &Connection, repository: &str, episode: &str) -> Result<&'static str> {
    let states: Vec<String> = rows(
        db,
        "SELECT state FROM am_jobs WHERE repository_id=? AND kind='project' AND json_extract(payload,'$.episode')=? ORDER BY created_at DESC,rowid DESC",
        params![repository, episode],
    )?
    .iter()
    .map(|row| s(row, "state").to_string())
    .collect();
    Ok(if states.iter().any(|state| state == "running") {
        "running"
    } else if states
        .iter()
        .any(|state| state == "queued" || state == "retry")
    {
        "queued"
    } else if states.first().is_some_and(|state| state == "failed") {
        "failed"
    } else {
        "none"
    })
}

/// Queue a graph projection of the repository's current head set, labelled
/// with the commit identity when the sync metadata carries one. One job per
/// head-set episode: re-committing identical content replays the existing job.
/// A sync is a request for the head set's graph, so while the
/// episode is not recorded it gives that job a fresh start when it failed,
/// was cancelled or finished as a skip, and lets one that waits to retry run
/// at once. A restarted job runs as the syncing principal, and so does a
/// waiting one whose principal can no longer read the project (a revoked or
/// expired key would have it cancelled when leased).
pub fn enqueue_projection(
    db: &Connection,
    repository: &str,
    p: &Principal,
    snapshot: &str,
    metadata: &Value,
) -> Result<Value> {
    let episode = crate::graph_projection::current_episode(db, repository)?;
    let mut labels: Vec<String> = Vec::new();
    for key in ["commit", "commit_sha", "sha", "revision", "ref"] {
        if let Some(label) = metadata[key]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 128)
            && !labels.contains(&label.to_string())
        {
            labels.push(label.to_string());
        }
    }
    if !snapshot.is_empty() && !labels.contains(&snapshot.to_string()) {
        labels.push(snapshot.to_string());
    }
    let input_hash = projection_input_hash(&episode);
    if let Some(mut existing) = one(
        db,
        "SELECT id,state,result,payload,principal_hash FROM am_jobs WHERE repository_id=? AND kind='project' AND input_hash=?",
        params![repository, input_hash],
    )? {
        // Fold in any new label so a later commit of identical content is still named.
        decode(&mut existing, "payload")?;
        let mut payload = existing["payload"].clone();
        let mut known: Vec<String> = payload["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut grew = false;
        let mut state = existing["state"].clone();
        for label in &labels {
            if !known.contains(label) {
                known.push(label.clone());
                grew = true;
            }
        }
        if grew {
            payload["labels"] = json!(known);
            db.execute(
                "UPDATE am_jobs SET payload=?,updated_at=? WHERE id=?",
                params![payload.to_string(), now(), s(&existing, "id")],
            )?;
        }
        if crate::graph_projection::is_recorded(db, repository, &episode)? {
            // A sync back to a recorded head set queues no
            // projection, so the live-symbol table follows it here. A
            // failure only leaves the table behind, which the symbol check
            // reads as `projection_pending`; the sync goes on.
            let _ = crate::symbols::ensure(db, repository, &episode);
            if grew {
                // The episode already exists; attach the new labels directly.
                for label in &labels {
                    crate::graph_projection::label_episode(
                        db,
                        &p.workspace,
                        repository,
                        label,
                        &episode,
                    )?;
                }
            }
        } else {
            // The principal a waiting job keeps: its own while it can still
            // read the project, else this sync's, so that leasing it does not
            // cancel it ("Credential unavailable") and leave the head set
            // without its episode for good.
            let waiting_principal = || -> Result<String> {
                let original = s(&existing, "principal_hash");
                let readable = authorize_hash(db, &p.workspace, original, permission("project")?)
                    .and_then(|principal| {
                        crate::graph_projection::repo_readable(db, &principal, repository)
                    })
                    .unwrap_or(false);
                Ok(if readable {
                    original.to_string()
                } else {
                    p.hash.clone()
                })
            };
            match s(&existing, "state") {
                // A sync is a request for its head set's graph. A
                // job that failed or was cancelled starts again with a fresh
                // budget, and one that finished as a skip (the head set had
                // moved on before it ran) runs again, with every label, now the
                // tree is current again. It runs as this sync's principal.
                "failed" | "cancelled" | "complete" => {
                    db.execute(
                        "UPDATE am_jobs SET state='queued',attempt=0,not_before=?,updated_at=?,error=NULL,lease_token=NULL,lease_until=NULL,principal_hash=? WHERE id=?",
                        params![now(), now(), p.hash, s(&existing, "id")],
                    )?;
                    state = json!("queued");
                }
                // One that waits to retry may run at once.
                "retry" => {
                    db.execute(
                        "UPDATE am_jobs SET not_before=?,updated_at=?,principal_hash=? WHERE id=?",
                        params![now(), now(), waiting_principal()?, s(&existing, "id")],
                    )?;
                }
                "queued" => {
                    let principal = waiting_principal()?;
                    if principal != s(&existing, "principal_hash") {
                        db.execute(
                            "UPDATE am_jobs SET updated_at=?,principal_hash=? WHERE id=?",
                            params![now(), principal, s(&existing, "id")],
                        )?;
                    }
                }
                _ => {}
            }
        }
        return Ok(
            json!({"id":existing["id"],"state":state,"kind":"project","episode":episode,"replayed":true}),
        );
    }
    let job_id = id();
    let stamp = now();
    let repository_row = repo(db, p, repository)?;
    let source_state = memory::state(&memory::documents(db, &repository_row, p)?);
    let payload = json!({"kind":"project","episode":episode,"labels":labels,"snapshot":snapshot,"runtime_revision":runtime_revision(),"source_state":source_state});
    let encoded = payload.to_string();
    crate::ingestion::check_queue_bytes(
        db,
        repository,
        encoded.len(),
        crate::ingestion::DEFAULT_QUEUED_BYTES,
    )?;
    // Background derivation: never ahead of work a caller is waiting on.
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,'project',?,?,?,'queued',-1,3,?,?,?)",
        params![job_id, repository, input_hash, encoded, p.hash, stamp, stamp, stamp],
    )?;
    store::event(
        db,
        repository,
        "workflow_queued",
        json!({"id":job_id,"kind":"project","episode":episode}),
    )?;
    Ok(json!({"id":job_id,"state":"queued","kind":"project","episode":episode,"replayed":false}))
}

/// How often a running `project` or `link` job renews its 240 s lease
///: a projection that takes longer than the lease is then
/// neither failed by its own lease check nor taken over by another process.
pub const LEASE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `work` while a thread renews `job`'s lease every `every` ([`renew`]:
/// only while the job is still `running` under this lease, so a job that was
/// cancelled or taken over is left alone). A beat that finds the store busy
/// for a second is skipped; the next one tries again. Returns once `work`
/// has returned and the heartbeat has stopped.
pub fn with_lease_heartbeat<T>(
    store: &Store,
    job: &Job,
    every: std::time::Duration,
    work: impl FnOnce() -> T,
) -> T {
    let (done, beats) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = beats.recv_timeout(every) {
                if let Ok(db) = store.connect() {
                    let _ = db.busy_timeout(std::time::Duration::from_secs(1));
                    let _ = renew(&db, job);
                }
            }
        });
        let value = work();
        drop(done);
        value
    })
}

/// Execute a `project` job: build (or replay) the episode for the head set
/// recorded at enqueue time and attach its labels. Skips when the head set
/// moved on — the newer commit queued its own projection. The job's lease
/// is renewed while it runs ([`with_lease_heartbeat`]).
pub fn run_projection(store: &Store, job: &Job) -> Result<Value> {
    with_lease_heartbeat(store, job, LEASE_HEARTBEAT, || project_job(store, job))
}

fn project_job(store: &Store, job: &Job) -> Result<Value> {
    let expected = s(&job.payload, "episode").to_string();
    let labels: Vec<String> = job.payload["labels"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let scope = |db: &Connection| -> Result<(Principal, String, String)> {
        let row = one(
            db,
            "SELECT state,lease_token,lease_until FROM am_jobs WHERE id=?",
            [&job.id],
        )?
        .ok_or_else(|| Error::conflict("Job was removed"))?;
        if row["state"] != "running"
            || s(&row, "lease_token") != job.lease
            || s(&row, "lease_until") < now().as_str()
        {
            return Err(Error::conflict("Job lease expired or was cancelled"));
        }
        let p = authorize_hash(db, &job.workspace, &job.principal_hash, "read")?;
        let identifier = s(&repo(db, &p, &job.repository)?, "id").to_string();
        let current = crate::graph_projection::current_episode(db, &identifier)?;
        Ok((p, identifier, current))
    };
    let skipped = |current: &str| json!({"skipped":true,"reason":"head set changed before projection","episode":expected,"current_episode":current});
    // Build on a read-only snapshot: extraction never holds the write lock,
    // so other writers are not kept waiting while a large repository parses.
    let built = store.read(|db| {
        let (_, identifier, current) = scope(db)?;
        if current != expected {
            return Ok(Err(current));
        }
        Ok(Ok(crate::graph_projection::prebuild(db, &identifier)?))
    })?;
    let built = match built {
        Ok(built) => built,
        Err(current) => return Ok(skipped(&current)),
    };
    store.transaction(|db| {
        let (p, identifier, current) = scope(db)?;
        let identifier = identifier.as_str();
        if current != expected {
            return Ok(skipped(&current));
        }
        let info = crate::graph_projection::project_prebuilt(db, identifier, &p, built)?;
        for label in &labels {
            crate::graph_projection::label_episode(
                db,
                &p.workspace,
                identifier,
                label,
                &info.episode,
            )?;
        }
        let mut out = serde_json::to_value(&info)?;
        out["labels"] = json!(labels);
        // Cross-repository import linkage follows every projection as its own
        // `link` job so the workspace's `gf_cross_edges` track the new episode
        // without extending the projection transaction; a failure to queue it
        // must not fail the projection itself.
        out["cross_repo_link"] = match enqueue_link(db, identifier, &p, &info.episode) {
            Ok(queued) => queued,
            Err(error) => json!({"error": error.message}),
        };
        // Work extensions queue after a projection, which the projection
        // drain runs next. Listed only when an
        // extension queued something, so Core's result is unchanged.
        let followed =
            crate::extension::installed().after_projection(db, &p, identifier, &info.episode);
        if !followed.is_empty() {
            out["after_projection"] = json!(followed);
        }
        // Bound the per-episode by-products (coverage, closed cross edges,
        // unresolved calls) to the configured retention; never nodes/edges.
        // A trim that removed rows is receipted like any other deletion.
        let retention = crate::retention::Retention::from_env();
        out["retention"] = match crate::retention::apply(db, identifier, &retention) {
            Ok(applied) => {
                let removed = applied["deleted"].clone();
                let any = removed
                    .as_object()
                    .is_some_and(|counts| counts.values().any(|n| n.as_u64().unwrap_or(0) > 0));
                if any {
                    let signed = crate::retention::remaining(db, identifier, &retention).and_then(
                        |remaining| {
                            crate::deletion::rows_receipt(
                                db,
                                &p,
                                identifier,
                                "retention.graph",
                                "graph_by_products",
                                json!({"retention": retention, "trigger": "projection"}),
                                removed.clone(),
                                &[],
                                remaining,
                                false,
                            )
                        },
                    );
                    out["retention_receipt"] = match signed {
                        Ok(receipt) => receipt["id"].clone(),
                        Err(error) => json!({"error": error.message}),
                    };
                }
                removed
            }
            Err(error) => json!({"error": error.message}),
        };
        Ok(out)
    })
}

/// Queue one cross-repository `link` run for the workspace after `episode`
/// of `repository` was recorded, or after a connector sync or import changed
/// its documents. Idempotent per workspace: a `link`
/// job that is still waiting (queued or retry) covers every later change
/// too, so it is replayed instead of duplicated; a job already running is
/// *not* reused, because its pass may have read the store before this
/// change. A running job with this very input (a connector sync leaves the
/// episode as it was) gets a follow-up job of its own, which waits for it.
pub fn enqueue_link(
    db: &Connection,
    repository: &str,
    p: &Principal,
    episode: &str,
) -> Result<Value> {
    if let Some(waiting) = one(
        db,
        "SELECT j.id,j.state FROM am_jobs j JOIN repo_projects r ON r.id=j.repository_id \
         WHERE r.workspace_id=? AND j.kind='link' AND j.state IN ('queued','retry') ORDER BY j.created_at LIMIT 1",
        [&p.workspace],
    )? {
        return Ok(
            json!({"id":waiting["id"],"state":waiting["state"],"kind":"link","replayed":true}),
        );
    }
    let mut input =
        json!({"kind":"link","workspace":p.workspace,"trigger":repository,"episode":episode});
    let stamp = now();
    let mut follows = None;
    // At most two rounds: a job with this input, then its follow-up, which
    // cannot be running while the first is (one job runs per project).
    for _ in 0..2 {
        let input_hash = digest(&input);
        let Some(existing) = one(
            db,
            "SELECT id,state FROM am_jobs WHERE repository_id=? AND kind='link' AND input_hash=?",
            params![repository, input_hash],
        )?
        else {
            break;
        };
        if existing["state"] == "running" {
            follows = Some(s(&existing, "id").to_string());
            input["after"] = existing["id"].clone();
            continue;
        }
        db.execute(
            "UPDATE am_jobs SET state='queued',not_before=?,updated_at=?,error=NULL,attempt=0 WHERE id=?",
            params![stamp, stamp, s(&existing, "id")],
        )?;
        return Ok(json!({"id":existing["id"],"state":"queued","kind":"link","replayed":true}));
    }
    let input_hash = digest(&input);
    let job_id = id();
    let mut payload = json!({"kind":"link","workspace":p.workspace,"trigger":repository,"episode":episode,"runtime_revision":runtime_revision()});
    if let Some(running) = &follows {
        payload["after"] = json!(running);
    }
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,'link',?,?,?,'queued',-1,3,?,?,?)",
        params![job_id, repository, input_hash, payload.to_string(), p.hash, stamp, stamp, stamp],
    )?;
    store::event(
        db,
        repository,
        "workflow_queued",
        json!({"id":job_id,"kind":"link","episode":episode}),
    )?;
    Ok(json!({"id":job_id,"state":"queued","kind":"link","replayed":false}))
}

/// Execute a `link` job: resolve imports between every readable, projected
/// repository of the workspace and persist `gf_cross_edges` /
/// `gf_cross_unresolved` (idempotent for unchanged episode pairs), then
/// recompute the documents' mention links to code symbols
/// ([`crate::doc_links::relink`]), whose summary is
/// the result's `documents`. A failure of the document step is reported
/// there and leaves the import links in place; a failure of the import step
/// still fails the job, after the document step has run. The job's lease is
/// renewed while it waits for the store and runs.
pub fn run_link(store: &Store, job: &Job) -> Result<Value> {
    with_lease_heartbeat(store, job, LEASE_HEARTBEAT, || link_job(store, job))
}

/// The job still holds its lease (checked inside a write transaction).
fn leased(db: &Connection, job: &Job) -> Result<()> {
    let row = one(
        db,
        "SELECT state,lease_token,lease_until FROM am_jobs WHERE id=?",
        [&job.id],
    )?
    .ok_or_else(|| Error::conflict("Job was removed"))?;
    if row["state"] != "running"
        || s(&row, "lease_token") != job.lease
        || s(&row, "lease_until") < now().as_str()
    {
        return Err(Error::conflict("Job lease expired or was cancelled"));
    }
    Ok(())
}

fn link_job(store: &Store, job: &Job) -> Result<Value> {
    // The document step runs whatever the import step did: a project whose
    // graph cannot be read fails the import step for the workspace, but
    // leaves the other projects' documents linking.
    let imports = import_link_job(store, job);
    let documents =
        match crate::doc_links::relink(store, &job.workspace, &job.principal_hash, |db| {
            leased(db, job)
        }) {
            Ok(summary) => summary,
            Err(error) => json!({"error": error.message}),
        };
    let mut result = imports?;
    result["documents"] = documents;
    Ok(result)
}

fn import_link_job(store: &Store, job: &Job) -> Result<Value> {
    store.transaction(|db| {
        leased(db, job)?;
        let p = authorize_hash(db, &job.workspace, &job.principal_hash, "read")?;
        let summary = crate::graph_projection::link_workspace(db, &p)?;
        let mut result = json!({
            "edges": summary["edges"],
            "unresolved": summary["unresolved"],
            "pairs": summary["pairs"].as_array().map_or(0, Vec::len),
            "repositories": summary["repositories"].as_array().map_or(0, Vec::len),
            "hidden_repositories": summary["hidden_repositories"],
            "unprojected_repositories": summary["unprojected_repositories"],
            "relinked": summary["relinked"],
            "reused": summary["reused"],
            "trigger": job.payload["trigger"],
            "episode": job.payload["episode"],
        });
        if let Some(skipped) = summary.get("skipped") {
            result["skipped"] = skipped.clone();
        }
        Ok(result)
    })
}

/// Run `repository`'s waiting graph work in this
/// process, as a worker would. First every ready `project` job, then the
/// `link` jobs they queued (a workspace keeps one waiting `link` job, which
/// may belong to another of its projects), then the jobs of the kinds
/// extensions declare to follow projections
/// ([`crate::extension::JobKind::after_projection`]). `graphicle sync`, `mcp`,
/// `ask`, `run --wait` and `pr` call it; a query never does.
///
/// Leases make it safe beside a worker or another drain: a job another
/// process holds is left to it and never run twice. Returns one summary per
/// job run (`{job, kind, ok, ...}`). A `project` job that fails ends the
/// drain with its error; the job is set to retry or failed as a worker sets
/// it.
pub fn drain_projection(store: &Store, repository: &str) -> Result<Vec<Value>> {
    drain_projection_until(store, repository, None)
}

/// [`drain_projection`] that also stops between jobs once `stop` is set
/// (`graphicle mcp` sets it when its client disconnects).
pub fn drain_projection_until(
    store: &Store,
    repository: &str,
    stop: Option<&AtomicBool>,
) -> Result<Vec<Value>> {
    let stopped = || stop.is_some_and(|flag| flag.load(Ordering::Relaxed));
    let mut results = Vec::new();
    // Projects whose `link` jobs run here: this one, and the owner of each
    // waiting `link` job a projection replayed.
    let mut linked = vec![repository.to_string()];
    while !stopped() {
        let Some(job) = store.lease_job_of_kind(repository, "project")? else {
            break;
        };
        let result = crate::phase::phase("job_project", || run_projection(store, &job));
        let (summary, link) = match &result {
            Ok(value) => (
                json!({"job":job.id,"kind":"project","ok":true,"labels":value["labels"]}),
                value["cross_repo_link"]["id"].as_str().map(str::to_string),
            ),
            Err(error) => (
                json!({"job":job.id,"kind":"project","ok":false,"error":error.message}),
                None,
            ),
        };
        let failed = result.as_ref().err().map(|error| error.message.clone());
        // Finish the job before anything else can fail, so its lease is
        // released rather than left to expire.
        store.finish_job(&job, result)?;
        results.push(summary);
        if let Some(error) = failed {
            return Err(Error::bad(format!("Graph projection failed: {error}")));
        }
        if let Some(link) = link
            && let Some(owner) = store.read(|db| {
                Ok(
                    one(db, "SELECT repository_id FROM am_jobs WHERE id=?", [&link])?
                        .map(|row| s(&row, "repository_id").to_string()),
                )
            })?
            && !linked.contains(&owner)
        {
            linked.push(owner);
        }
    }
    for owner in &linked {
        while !stopped() {
            let Some(job) = store.lease_job_of_kind(owner, "link")? else {
                break;
            };
            let result = crate::phase::phase("job_link", || run_link(store, &job));
            results.push(match &result {
                Ok(value) => {
                    json!({"job":job.id,"kind":"link","ok":true,"edges":value["edges"],"unresolved":value["unresolved"]})
                }
                Err(error) => json!({"job":job.id,"kind":"link","ok":false,"error":error.message}),
            });
            store.finish_job(&job, result)?;
        }
    }
    let kinds = crate::extension::installed().post_projection_kinds();
    if !kinds.is_empty() && !stopped() {
        // Post-projection kinds make no model call (`JobKind::after_projection`).
        let worker = Worker::new(store.clone(), model::Config::default())?;
        for kind in kinds {
            while !stopped() {
                let Some(job) = store.lease_job_of_kind(repository, kind)? else {
                    break;
                };
                let result = run_on_own_runtime(&worker, &job);
                results.push(match &result {
                    Ok(_) => json!({"job":job.id,"kind":kind,"ok":true}),
                    Err(error) => {
                        json!({"job":job.id,"kind":kind,"ok":false,"error":error.message})
                    }
                });
                store.finish_job(&job, result)?;
            }
        }
    }
    Ok(results)
}

/// Whether `error` is SQLite's busy or locked error: another connection
/// held the write lock past the store's busy timeout (10 s). The store is
/// busy with another writer, such as another process persisting a large
/// projection; the work itself did not fail. The storage
/// error keeps only SQLite's text, in its two forms.
pub fn busy(error: &Error) -> bool {
    let message = error.message.to_ascii_lowercase();
    [
        "database is locked",
        "database table is locked",
        "database file is locked",
        "table in the database is locked",
    ]
    .iter()
    .any(|text| message.contains(text))
}

/// Run a leased job through [`crate::workflows::execute`] on a thread of its
/// own with a current-thread runtime, so synchronous callers and callers
/// inside the program's runtime can both wait for it.
fn run_on_own_runtime(worker: &Worker, job: &Job) -> Result<Value> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(crate::workflows::execute(worker, job))
            })
            .join()
            .unwrap_or_else(|_| Err(Error::bad("A job that follows the projection panicked")))
    })
}
#[track_caller]
pub fn guarded<T>(
    store: &Store,
    job: &Job,
    f: impl FnOnce(&Connection, &Principal, &Value) -> Result<T>,
) -> Result<T> {
    store.transaction(|db| {
        let (p, repository) = still_valid(db, job)?;
        f(db, &p, &repository)
    })
}
/// `guarded` for steps that only read: the same lease, authority and source
/// checks on a read-only snapshot, so the step never holds the write lock.
pub fn guarded_read<T>(
    store: &Store,
    job: &Job,
    f: impl FnOnce(&Connection, &Principal, &Value) -> Result<T>,
) -> Result<T> {
    store.read(|db| {
        let (p, repository) = still_valid(db, job)?;
        f(db, &p, &repository)
    })
}
/// The job still holds its lease, its principal still has the permission, and
/// the sources and reviewed memory it was queued against are unchanged.
fn still_valid(db: &Connection, job: &Job) -> Result<(Principal, Value)> {
    let row = one(
        db,
        "SELECT state,lease_token,lease_until FROM am_jobs WHERE id=?",
        [&job.id],
    )?
    .ok_or_else(|| Error::conflict("Job was removed"))?;
    if row["state"] != "running"
        || s(&row, "lease_token") != job.lease
        || s(&row, "lease_until") < now().as_str()
    {
        return Err(Error::conflict("Job lease expired or was cancelled"));
    }
    let p = authorize_hash(
        db,
        &job.workspace,
        &job.principal_hash,
        permission(&job.kind)?,
    )?;
    let repository = repo(db, &p, &job.repository)?;
    if !source_task(&job.kind) {
        // One read of the sources serves both checks.
        let documents = memory::documents(db, &repository, &p)?;
        if memory::state(&documents) != s(&job.payload, "source_state") {
            return Err(Error::conflict(
                "Sources or source access changed during the job",
            ));
        }
        if job.payload["memory_state"].is_string()
            && memory::reviewed_state_of(db, &repository, &documents)?
                != s(&job.payload, "memory_state")
        {
            return Err(Error::conflict("Reviewed memory changed during the job"));
        }
    }
    Ok((p, repository))
}

fn expire_leases(db: &Connection, stamp: &str) -> Result<()> {
    db.execute("UPDATE am_jobs SET state=CASE WHEN attempt<max_attempts THEN 'retry' ELSE 'failed' END,error='Worker lease expired; inference billing may be unknown',lease_token=NULL,lease_until=NULL,updated_at=? WHERE state='running' AND lease_until<?",params![stamp,stamp])?;
    Ok(())
}

/// Validate a workflow request before any database access; returns the
/// permission it needs. `config` is the workspace's effective configuration.
pub fn check(args: &Value, config: &model::Config) -> Result<&'static str> {
    let kind = string(args, "kind", 30)?;
    crate::archived::check(crate::archived::Surface::Workflow, args)?;
    if crate::ingestion::interactive(kind) {
        return Err(Error::bad(format!(
            "{kind} is an interactive operation; call it directly, it is never queued"
        )));
    }
    let permission = permission(kind)?;
    crate::workflow_retrieval::check(kind, args, config)?;
    if kind == crate::observer::KIND {
        crate::observer::validate_args(args)?;
    }
    if kind != crate::observer::KIND && args.get("parallel_sources").is_some() {
        return Err(Error::bad(
            "parallel_sources applies only to observer_notes",
        ));
    }
    if kind == "basic" {
        crate::query::Filter::parse(args)?;
    }
    // An answer job reads one project. Workspace scope is a
    // recall over several (`context`, MCP `recall_project`); an answer over
    // several projects would need their combined source state pinned, the
    // model's citations qualified by project, and grounding across them.
    if crate::workspace::Scope::parse(args)? == crate::workspace::Scope::Workspace
        || args.get("max_projects").is_some()
    {
        return Err(Error::bad(
            "scope workspace applies to context and recall_project; a workflow reads one project",
        ));
    }
    // Observer-note reading options apply to basic answers only; the notes
    // state is pinned here, never by the caller.
    if kind != "basic"
        && let Some(field) = ["observer_notes", "notes_tokens", "answer_contract"]
            .into_iter()
            .find(|f| args.get(*f).is_some())
    {
        return Err(Error::bad(format!(
            "{field} applies only to basic question answering"
        )));
    }
    if let Some(variants) = args.get("query_variants") {
        if !["basic", "recall"].contains(&kind) {
            return Err(Error::bad("query_variants apply only to basic and recall"));
        }
        graphicle_memory::retrieval::query_variants(variants).map_err(Error::from)?;
    }
    if args.get("note_keys").is_some() && !["basic", "recall"].contains(&kind) {
        return Err(Error::bad("note_keys applies only to basic and recall"));
    }
    let note_keys = crate::observer::KeyMode::parse(args.get("note_keys"))?.is_some();
    if args.get("auto").is_some() && kind != crate::observer::KIND {
        return Err(Error::bad("auto applies only to observer notes"));
    }
    if args.get("answer_contract").is_some_and(|v| {
        !v.as_str()
            .is_some_and(|c| crate::search::CONTRACTS.contains(&c))
    }) {
        return Err(Error::bad("Unknown answer_contract"));
    }
    if args.get("notes_state").is_some() {
        return Err(Error::bad("notes_state is pinned by the service"));
    }
    // grounded-v3 reads observer notes unless the caller turns them off,
    // and budgets them beside the evidence rather than inside it.
    let v3 = args["answer_contract"] == "grounded-v3";
    let reads_notes =
        args["observer_notes"] == true || (v3 && args.get("observer_notes").is_none());
    if args.get("notes_tokens").is_some() && !reads_notes {
        return Err(Error::bad("notes_tokens requires observer_notes"));
    }
    if kind == "basic" && note_keys && !reads_notes {
        return Err(Error::bad("note_keys requires observer_notes"));
    }
    integer(args, "notes_tokens", 0, if v3 { 64000 } else { 16000 })?;
    for field in [
        "memory_retrieval",
        "graph_retrieval",
        "doc_links",
        "symbol_links",
        "notes_first",
        "observer_notes",
        "verify_answer",
    ] {
        if args.get(field).is_some_and(|v| !v.is_boolean()) {
            return Err(Error::bad(format!("{field} must be true or false")));
        }
    }
    for (field, choices) in [
        ("dispatch", ["local", "redis"]),
        ("vector_backend", ["local", "qdrant"]),
    ] {
        if args
            .get(field)
            .is_some_and(|value| !value.as_str().is_some_and(|v| choices.contains(&v)))
        {
            return Err(Error::bad(format!("Unknown {field}")));
        }
    }

    let selected_model = args["model"]
        .as_str()
        .or_else(|| {
            (kind == "recall")
                .then(|| args["local_embedding_profile"].as_str())
                .flatten()
        })
        .or(if ["embed", "recall"].contains(&kind) {
            config.embedding_model.as_deref()
        } else {
            None
        });
    if !source_task(kind) {
        config.profile(selected_model)?;
    }
    Ok(permission)
}

/// Queue a checked workflow for `p` inside the caller's transaction.
pub fn queue(
    db: &Connection,
    p: &Principal,
    args: &Value,
    config: &model::Config,
) -> Result<Value> {
    let kind = string(args, "kind", 30)?;
    let bounds = crate::ingestion::Bounds::from_config(config);
    let note_keys = crate::observer::KeyMode::parse(args.get("note_keys"))?.is_some();
    let selected_model = args["model"]
        .as_str()
        .or_else(|| {
            (kind == "recall")
                .then(|| args["local_embedding_profile"].as_str())
                .flatten()
        })
        .or(if ["embed", "recall"].contains(&kind) {
            config.embedding_model.as_deref()
        } else {
            None
        });
    let repository = repo(db, p, string(args, "repository_id", 100)?)?;
    let identifier = s(&repository, "id");
    if let Some(reason) = unavailable(db, identifier, kind, args)? {
        return Err(Error::bad(reason));
    }
    if args.to_string().len() > 65536 {
        return Err(Error::bad("Workflow arguments exceed 64 KiB"));
    }
    let max_calls = integer(args, "max_calls", 4, 32)?.max(1);
    let max_input = integer(args, "max_input_tokens", 64000, 512000)?.max(512);
    let max_output = integer(args, "max_output_tokens", 8192, 64000)?.max(64);
    let max_attempts = integer(args, "max_attempts", 1, 3)?.max(1);
    let mut payload = args.clone();
    payload["max_calls"] = json!(max_calls);
    payload["max_input_tokens"] = json!(max_input);
    payload["max_output_tokens"] = json!(max_output);
    payload["runtime_revision"] = json!(runtime_revision());
    crate::workflow_retrieval::pin(&mut payload, config)?;
    let documents = memory::documents(db, &repository, p)?;
    payload["source_state"] = json!(memory::state(&documents));
    payload["memory_state"] = json!(memory::reviewed_state_of(db, &repository, &documents)?);
    if !source_task(kind) {
        let (name, profile) = config.profile(selected_model)?;
        payload["model"] = json!(name);
        payload["model_key"] = json!(model::key(profile));
    }
    let v3 = kind == "basic" && args["answer_contract"] == "grounded-v3";
    if v3 && args.get("observer_notes").is_none() {
        payload["observer_notes"] = json!(true);
    }
    if (kind == "basic" && payload["observer_notes"] == true) || (kind == "recall" && note_keys) {
        payload["notes_state"] = json!(crate::observer::state(db, identifier, &documents)?);
    }
    if kind == "basic" {
        payload["answer_contract"] = json!(
            args["answer_contract"]
                .as_str()
                .unwrap_or(crate::search::DEFAULT_CONTRACT)
        );
    }
    // A source scan owns its cursor until completion or cancellation.
    // Idempotence alone is insufficient when two callers use different budgets.
    if source_task(kind) {
        let connector_id = string(args, "connector_id", 100)?;
        for mut active in rows(
            db,
            "SELECT id,state,payload FROM am_jobs WHERE repository_id=? AND kind=? AND state IN ('queued','retry','running')",
            params![identifier, kind],
        )? {
            decode(&mut active, "payload")?;
            if active["payload"]["connector_id"] == connector_id {
                return Ok(
                    json!({"id":active["id"],"state":active["state"],"kind":kind,"replayed":true}),
                );
            }
        }
    }
    if ["basic", "reflect"].contains(&kind)
        && args["semantic_retrieval"] != false
        && let Some(name) = args["embedding_model"]
            .as_str()
            .or(args["local_embedding_profile"].as_str())
            .or(config.embedding_model.as_deref())
    {
        let (_, profile) = config.profile(Some(name))?;
        payload["embedding_model"] = json!(name);
        payload["embedding_key"] = json!(model::key(profile));
    }
    let progress = match kind {
        "extract" => one(
            db,
            "SELECT count(*) AS count,max(created_at) AS latest FROM am_document_reads WHERE repository_id=?",
            [identifier],
        )?,
        "embed" => one(
            db,
            "SELECT count(*) AS count FROM am_vectors WHERE repository_id=?",
            [identifier],
        )?,
        crate::observer::KIND => one(
            db,
            "SELECT count(*) AS count,max(created_at) AS latest FROM am_observer_notes WHERE repository_id=?",
            [identifier],
        )?,
        "connector" | "permissions" | "connector_probe" => one(
            db,
            "SELECT generation,cursor FROM am_connectors WHERE repository_id=? AND id=?",
            params![identifier, string(args, "connector_id", 100)?],
        )?,
        _ => None,
    };
    payload["progress"] = json!(progress);
    if kind == "connector_probe" && payload["retry_of"].is_null() {
        payload["requested_at"] = json!(now());
    }
    let input_hash = digest(&json!({"payload":payload,"principal":p.hash}));
    if let Some(mut old) = one(
        db,
        "SELECT id,state,result,error FROM am_jobs WHERE repository_id=? AND kind=? AND input_hash=?",
        params![identifier, kind, input_hash],
    )? {
        decode(&mut old, "result")?;
        old["replayed"] = json!(true);
        return Ok(old);
    }
    let active:i64=db.query_row("SELECT count(*) FROM am_jobs WHERE repository_id=? AND state IN ('queued','retry','running')",[identifier],|r|r.get(0))?;
    if active >= 100 {
        return Err(Error::bad("Project queue is full"));
    }
    let encoded = payload.to_string();
    crate::ingestion::check_queue_bytes(
        db,
        identifier,
        encoded.len(),
        bounds.max_queued_bytes_per_repository,
    )?;
    let identifier_job = id();
    let stamp = now();
    db.execute("INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,max_attempts,not_before,created_at,updated_at) VALUES(?,?,?,?,?,?,'queued',?,?,?,?)",params![identifier_job,identifier,kind,input_hash,encoded,p.hash,max_attempts,stamp,stamp,stamp])?;
    store::event(
        db,
        identifier,
        "workflow_queued",
        json!({"id":identifier_job,"kind":kind}),
    )?;
    Ok(
        json!({"id":identifier_job,"state":"queued","kind":kind,"limits":{"calls":max_calls,"input_tokens":max_input,"output_tokens":max_output},"replayed":false}),
    )
}

impl Store {
    pub fn enqueue(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        config: &model::Config,
    ) -> Result<Value> {
        let config = crate::settings::configuration(self, workspace, config)?;
        // Automatic observation rounds are marked by the service, never a caller.
        if args.get("auto").is_some() {
            return Err(Error::bad("auto is set by the service"));
        }
        let permission = check(args, &config)?;
        self.authorized(workspace, token, permission, |db, p| {
            queue(db, p, args, &config)
        })
        .inspect(|job| {
            if job["replayed"] == false {
                self.wake.notify_one();
            }
        })
    }
    pub fn job_status(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace,token,"read",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let mut result=Vec::new();
            // The caller's source view and reviewed memory, read once per call.
            let mut documents:Option<Vec<Value>>=None;
            let mut sources:Option<String>=None;
            let mut reviewed:Option<String>=None;
            for mut row in rows(db,"SELECT id,kind,state,payload,principal_hash,attempt,result,error,created_at,updated_at FROM am_jobs WHERE repository_id=? ORDER BY created_at DESC LIMIT 100",[s(&repository,"id")])? {
                if p.role!="owner" && s(&row,"principal_hash")!=p.hash {continue;}
                if args["job_id"].as_str().is_some_and(|j|s(&row,"id")!=j) {continue;}
                decode(&mut row,"result")?;decode(&mut row,"payload")?;
                // Work this program cannot run waits for one that can.
                if ["queued","retry"].contains(&s(&row,"state")) && let Some(reason)=unavailable(db,s(&repository,"id"),s(&row,"kind"),&row["payload"])? {row["waiting_for"]=json!(reason);}
                // An unsupported kind names its unit, whatever its state.
                if let Some(archived)=crate::archived::kind(s(&row,"kind")) {row["archived"]=json!(archived.listing());}
                // Job answers can include private evidence. Recheck the caller's
                // entire source view even after the job has finished.
                if !source_task(s(&row,"kind")) {
                    if documents.is_none() {documents=Some(memory::documents(db,&repository,p)?);}
                    let view=documents.as_deref().unwrap_or_default();
                    if sources.is_none() {sources=Some(memory::state(view));}
                    if Some(s(&row["payload"],"source_state"))!=sources.as_deref() {
                        row["result"]=Value::Null;row["result_unavailable"]=json!("source_view_changed");
                    }
                    if row["payload"]["memory_state"].is_string() {
                        if reviewed.is_none() {reviewed=Some(memory::reviewed_state_of(db,&repository,view)?);}
                        if Some(s(&row["payload"],"memory_state"))!=reviewed.as_deref() {
                            row["result"]=Value::Null;row["result_unavailable"]=json!("reviewed_memory_changed");
                        }
                    }
                }
                let calls=rows(db,"SELECT c.*,r.state AS call_state,r.usage_reported,r.error FROM am_model_calls c LEFT JOIN am_call_reservations r ON r.id=c.id WHERE c.job_id=? ORDER BY c.created_at",[s(&row,"id")])?;
                row["calls"]=json!(calls);row.as_object_mut().unwrap().remove("principal_hash");row.as_object_mut().unwrap().remove("payload");result.push(row);
            }
            if args["job_id"].is_string() {result.into_iter().next().ok_or_else(||Error::bad("Job unavailable"))}else{Ok(json!({"jobs":result}))}
        })
    }
    /// Explicitly retry a failed/cancelled job with a fresh bounded budget.
    /// The original attempt and its reported/unknown usage remain in history.
    pub fn retry_job(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        config: &model::Config,
    ) -> Result<Value> {
        let retry = self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let mut old = one(
                db,
                "SELECT * FROM am_jobs WHERE repository_id=? AND id=?",
                params![s(&repository, "id"), string(args, "job_id", 100)?],
            )?
            .ok_or_else(|| Error::bad("Job unavailable"))?;
            if p.role != "owner" && s(&old, "principal_hash") != p.hash {
                return Err(Error::denied());
            }
            if !crate::store::permitted(db, p, permission(s(&old, "kind"))?)? {
                return Err(Error::denied());
            }
            if !["failed", "cancelled"].contains(&s(&old, "state")) {
                return Err(Error::conflict(
                    "Only failed or cancelled work can be retried",
                ));
            }
            decode(&mut old, "payload")?;
            let mut payload = old["payload"].clone();
            payload["retry_of"] = old["id"].clone();
            payload["kind"] = old["kind"].clone();
            payload["repository_id"] = repository["id"].clone();
            // The child has its own idempotence key. Repeated clicks return it.
            for field in [
                "requested_at",
                "progress",
                "runtime_revision",
                "source_state",
                "memory_state",
                "model_key",
            ] {
                payload.as_object_mut().unwrap().remove(field);
            }
            Ok(payload)
        })?;
        let mut result = self.enqueue(workspace, token, &retry, config)?;
        result["retry_of"] = retry["retry_of"].clone();
        result["new_budget_authorized"] = json!(true);
        Ok(result)
    }
    pub fn cancel_job(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            repo(db, p, string(args, "repository_id", 100)?)?;
            let job_id = string(args, "job_id", 100)?;
            let job = one(
                db,
                "SELECT * FROM am_jobs WHERE repository_id=? AND id=?",
                params![s(args, "repository_id"), job_id],
            )?
            .ok_or_else(|| Error::bad("Job unavailable"))?;
            if p.role != "owner" && s(&job, "principal_hash") != p.hash {
                return Err(Error::denied());
            }
            if ["queued", "retry", "running"].contains(&s(&job, "state")) {
                db.execute(
                    "UPDATE am_jobs SET state='cancelled',updated_at=?,lease_token=NULL WHERE id=?",
                    params![now(), job_id],
                )?;
            }
            Ok(json!({"id":job_id,"cancelled":true,"in_flight_provider_request_may_finish":true}))
        })
    }
    pub fn lease_job(&self) -> Result<Option<Job>> {
        self.lease_job_excluding(&[])
    }
    /// Kind of the job `lease_job` would offer next (priority order, honouring
    /// `not_before` and source/repository serialization), without leasing it.
    pub fn peek_ready_kind(&self) -> Result<Option<String>> {
        let db = self.connect()?;
        let stamp = now();
        // A crashed worker may leave the only job running. Recover it before
        // admission checks, or an otherwise idle queue never retries it.
        // Ordinary idle polls remain reads and do not take the write lock.
        if one(
            &db,
            "SELECT 1 AS expired FROM am_jobs WHERE state='running' AND lease_until<? LIMIT 1",
            [&stamp],
        )?
        .is_some()
        {
            self.transaction(|db| expire_leases(db, &stamp))?;
        }
        Ok(one(&db, &READY_JOB_QUERY, [&stamp])?.map(|row| s(&row, "kind").to_string()))
    }
    /// Lease the next ready job whose kind is not in `excluded` (G08 admission).
    /// Running work blocks other jobs in the same repository. Only explicitly
    /// opted-in observer jobs with disjoint source prefixes may overlap.
    pub fn lease_job_excluding(&self, excluded: &[&str]) -> Result<Option<Job>> {
        self.lease_job_filtered(excluded, None)
    }
    /// Lease the next ready job of exactly `kind` for one `repository`, leaving
    /// every other queued job untouched (`graphicle pr` runs its own
    /// projections synchronously in a store that may hold other work).
    pub fn lease_job_of_kind(&self, repository: &str, kind: &str) -> Result<Option<Job>> {
        self.lease_job_filtered(&[], Some((repository, kind)))
    }
    fn lease_job_filtered(
        &self,
        excluded: &[&str],
        only: Option<(&str, &str)>,
    ) -> Result<Option<Job>> {
        // Work only an extension runs stays queued while none is installed.
        let provider_sync = crate::connectors::provider_permission_sync();
        let mut excluded = excluded.to_vec();
        if !provider_sync {
            excluded.push(PERMISSIONS);
        }
        self.transaction(|db| {
            let stamp=now();
            expire_leases(db, &stamp)?;
            // Apply admission/repository filters before the candidate cap so
            // unrelated ready jobs cannot hide all eligible work.
            for mut row in rows(db,&LEASE_JOB_QUERY,params![stamp,json!(excluded).to_string(),only.map(|v|v.0),only.map(|v|v.1),provider_sync,json!(known_kinds()).to_string()])? {
                // Known kinds only (?6); a kind unknown here stays queued.
                let Ok(permission)=permission(s(&row,"kind")) else {continue};
                if authorize_hash(db,s(&row,"workspace_id"),s(&row,"principal_hash"),permission).is_err() {
                    db.execute("UPDATE am_jobs SET state='cancelled',error='Credential unavailable',updated_at=? WHERE id=?",params![stamp,s(&row,"id")])?;continue;
                }
                decode(&mut row,"payload")?;let lease=id();
                let until=(chrono::Utc::now()+chrono::Duration::seconds(240)).to_rfc3339_opts(chrono::SecondsFormat::Micros,true);
                db.execute("UPDATE am_jobs SET state='running',attempt=attempt+1,lease_token=?,lease_until=?,updated_at=? WHERE id=?",params![lease,until,stamp,s(&row,"id")])?;
                return Ok(Some(Job{id:s(&row,"id").into(),repository:s(&row,"repository_id").into(),workspace:s(&row,"workspace_id").into(),principal_hash:s(&row,"principal_hash").into(),lease,kind:s(&row,"kind").into(),payload:row["payload"].clone()}));
            }
            Ok(None)
        })
    }
    pub fn finish_job(&self, job: &Job, result: Result<Value>) -> Result<()> {
        self.transaction(|db| {
            let Some(current)=one(db,"SELECT * FROM am_jobs WHERE id=?",[&job.id])? else{return Ok(())};
            if s(&current,"lease_token")!=job.lease || current["state"]!="running" {return Ok(());}
            let (state,value,error)=match result {Ok(value)=>("complete",Some(value.to_string()),None),Err(error)=>{
                db.execute("DELETE FROM am_model_cache WHERE repository_id=? AND request_hash IN (SELECT request_hash FROM am_model_calls WHERE job_id=?)",params![job.repository,job.id])?;
                let retry=current["attempt"].as_u64()<current["max_attempts"].as_u64() && error.status!=axum::http::StatusCode::CONFLICT && error.status!=axum::http::StatusCode::FORBIDDEN;
                (if retry {"retry"}else{"failed"},None,Some(error.message))
            }};
            // Exponential backoff per attempt (G08): 30 s, 60 s, 120 s, ... capped.
            let later=crate::ingestion::backoff_stamp(current["attempt"].as_u64().unwrap_or(1));
            db.execute("UPDATE am_jobs SET state=?,result=?,error=?,not_before=?,lease_token=NULL,lease_until=NULL,updated_at=? WHERE id=?",params![state,value,error,later,now(),job.id])?;
            store::event(db,&job.repository,"workflow_finished",json!({"id":job.id,"state":state}))?;Ok(())
        })
    }
}
pub fn renew(db: &Connection, job: &Job) -> Result<()> {
    let until = (chrono::Utc::now() + chrono::Duration::seconds(240))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    db.execute("UPDATE am_jobs SET lease_until=?,updated_at=? WHERE id=? AND lease_token=? AND state='running'",params![until,now(),job.id,job.lease])?;
    Ok(())
}
#[derive(Clone)]
pub struct Worker {
    pub store: Store,
    pub config: Arc<model::Config>,
    pub client: reqwest::Client,
}
impl Worker {
    pub fn new(store: Store, config: model::Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            store,
            config: Arc::new(config),
            client: model::client()?,
        })
    }
    pub async fn once(&self) -> Result<bool> {
        let maintenance = crate::model::client_with_timeout(std::time::Duration::from_secs(30))?;
        let cleaned = crate::projection::once(&self.store, &maintenance).await?;
        // G08 admission: lease only work whose CPU/model permit is in hand, so
        // a job never spends its lease waiting behind another worker.
        let bounds = crate::ingestion::Bounds::from_config(&self.config);
        let limiter = crate::ingestion::Limiter::global(&bounds);
        let (mut admission, excluded) = limiter.try_admit();
        let Some(next) = self.store.peek_ready_kind()? else {
            return Ok(cleaned);
        };
        // Priority order is preserved: when the head job needs a permit another
        // worker holds, wait for admission rather than skipping past it.
        let leased = if excluded.contains(&next.as_str()) {
            drop(admission);
            admission = limiter.admit().await;
            self.store.lease_job()?
        } else {
            self.store.lease_job_excluding(&excluded)?
        };
        let Some(job) = leased else {
            return Ok(cleaned);
        };
        admission.retain_for(&job.kind);
        let result = crate::workflows::execute(self, &job).await;
        drop(admission);
        self.store.finish_job(&job, result)?;
        // A finished automatic round or source sync may call for the next
        // observation round now rather than at the next scheduler poll.
        let connector =
            job.payload["auto"]["connector_id"]
                .as_str()
                .or(job.payload["connector_id"]
                    .as_str()
                    .filter(|_| job.kind == "connector"));
        if let Some(connector) = connector {
            self.store.auto_observe(&self.config, Some(connector)).ok();
        }
        Ok(true)
    }
    /// Whether `infer` would accept one more uncached call of this size, so a
    /// batch workflow can stop cleanly instead of failing on its budget.
    pub fn fits(&self, job: &Job, input_tokens: usize) -> Result<bool> {
        guarded_read(&self.store, job, |db, _, _| {
            let totals=one(db,"SELECT count(*) AS calls,coalesce(sum(input_reserved),0) AS input,coalesce(sum(output_reserved),0) AS output FROM am_call_reservations WHERE job_id=?",[&job.id])?.unwrap();
            let used = |key: &str| totals[key].as_u64().unwrap_or(0);
            Ok(
                used("calls") < job.payload["max_calls"].as_u64().unwrap_or(4)
                    && used("input") + input_tokens as u64
                        <= job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
                    && job.payload["max_output_tokens"]
                        .as_u64()
                        .unwrap_or(8192)
                        .saturating_sub(used("output"))
                        >= 64,
            )
        })
    }
    pub async fn infer(
        &self,
        job: &Job,
        instructions: &str,
        input: &Value,
        schema: &Value,
    ) -> Result<Value> {
        let config = crate::settings::configuration(&self.store, &job.workspace, &self.config)?;
        let (_, profile) = config.profile(job.payload["model"].as_str())?;
        if s(&job.payload, "model_key") != model::key(profile) {
            return Err(Error::conflict("Model configuration changed after queuing"));
        }
        let credential = profile
            .key_env
            .as_deref()
            .map(|reference| self.store.resolve_credential(&job.workspace, reference))
            .transpose()?;
        let request_hash = digest(
            &json!({"model":model::key(profile),"instructions":instructions,"input":input,"schema":schema,"scope":job.repository,"source_state":job.payload["source_state"]}),
        );
        let reservation = id();
        let input_tokens = model::request_tokens(instructions, input, schema);
        let mut output_limit = profile.output_limit;
        let cached = guarded(&self.store, job, |db, _, _| {
            renew(db, job)?;
            if let Some(row) = one(
                db,
                "SELECT result FROM am_model_cache WHERE repository_id=? AND request_hash=? AND source_state=?",
                params![
                    job.repository,
                    request_hash,
                    s(&job.payload, "source_state")
                ],
            )? {
                db.execute(
                    "INSERT INTO am_model_calls VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        reservation,
                        job.repository,
                        job.id,
                        profile.protocol,
                        profile.model,
                        request_hash,
                        0,
                        0,
                        0,
                        1,
                        now()
                    ],
                )?;
                return Ok(Some(
                    serde_json::from_str::<model::Reply>(s(&row, "result"))
                        .map_err(|_| Error::bad("Invalid model cache"))?,
                ));
            }
            let totals=one(db,"SELECT count(*) AS calls,coalesce(sum(input_reserved),0) AS input,coalesce(sum(output_reserved),0) AS output FROM am_call_reservations WHERE job_id=?",[&job.id])?.unwrap();
            let remaining = (job.payload["max_output_tokens"].as_u64().unwrap_or(8192) as usize)
                .saturating_sub(totals["output"].as_u64().unwrap_or(0) as usize);
            output_limit = output_limit.min(remaining);
            if totals["calls"].as_u64().unwrap_or(0)
                >= job.payload["max_calls"].as_u64().unwrap_or(4)
                || totals["input"].as_u64().unwrap_or(0) + input_tokens as u64
                    > job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
                || output_limit < 64
            {
                return Err(Error::bad(
                    "Workflow inference budget exhausted; completed work is retained",
                ));
            }
            db.execute("INSERT INTO am_call_reservations(id,job_id,request_hash,state,input_reserved,output_reserved,created_at) VALUES(?,?,?,'started',?,?,?)",params![reservation,job.id,request_hash,input_tokens,output_limit,now()])?;
            db.execute(
                "INSERT INTO am_model_calls VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    reservation,
                    job.repository,
                    job.id,
                    profile.protocol,
                    profile.model,
                    request_hash,
                    input_tokens,
                    output_limit,
                    0,
                    0,
                    now()
                ],
            )?;
            Ok(None)
        })?;
        if let Some(reply) = cached {
            let value = reply.json_for(schema)?;
            validate_output(schema, &value)?;
            return Ok(value);
        }
        let reply = if job.payload["dispatch"] == "redis" {
            crate::distributed::generate(
                self,
                job,
                profile,
                instructions,
                input,
                schema,
                output_limit,
            )
            .await
        } else {
            model::generate_with_credential(
                &self.client,
                profile,
                instructions,
                input,
                schema,
                output_limit,
                credential.as_deref(),
            )
            .await
        };
        // Record even a failed/unknown provider call independently of job access.
        self.store.transaction(|db| {
            match &reply {
                Ok(reply)=>{
                    db.execute("UPDATE am_call_reservations SET state='finished',input_reserved=?,output_reserved=?,usage_reported=?,finished_at=? WHERE id=?",params![reply.input_tokens,reply.output_tokens,reply.usage_reported,now(),reservation])?;
                    db.execute("UPDATE am_model_calls SET input_tokens=?,output_tokens=?,elapsed_ms=?,model=? WHERE id=?",params![reply.input_tokens,reply.output_tokens,reply.elapsed_ms,reply.model,reservation])?;
                },Err(error)=>{
                    let reported = error.provider_failure.as_ref().and_then(|failure|
                        failure.input_tokens.zip(failure.output_tokens));
                    if let Some((input, output)) = reported {
                        db.execute("UPDATE am_call_reservations SET state='failed',input_reserved=?,output_reserved=?,usage_reported=1,error=?,finished_at=? WHERE id=?",params![input,output,error.message,now(),reservation])?;
                        db.execute("UPDATE am_model_calls SET input_tokens=?,output_tokens=? WHERE id=?",params![input,output,reservation])?;
                    } else {
                        db.execute("UPDATE am_call_reservations SET state='unknown_usage',error=?,finished_at=? WHERE id=?",params![error.message,now(),reservation])?;
                    }
                    if let Some(failure) = &error.provider_failure {
                        db.execute("UPDATE am_model_calls SET elapsed_ms=?,model=? WHERE id=?",params![failure.elapsed_ms,failure.model,reservation])?;
                    }
                }
            }Ok(())
        })?;
        let reply = reply?;
        let raw = reply.json()?;
        let value = reply.json_for(schema)?;
        validate_output(schema, &value)?;
        guarded(&self.store, job, |db, _, _| {
            if value != raw {
                store::event(
                    db,
                    &job.repository,
                    "model_array_envelope_restored",
                    json!({"job_id":job.id,"request_hash":request_hash,"semantic_fields_changed":false}),
                )?;
            }
            db.execute(
                "INSERT OR REPLACE INTO am_model_cache VALUES(?,?,?,?,?)",
                params![
                    job.repository,
                    request_hash,
                    s(&job.payload, "source_state"),
                    serde_json::to_string(&reply)?,
                    now()
                ],
            )?;
            db.execute("DELETE FROM am_model_cache WHERE repository_id=? AND request_hash IN (SELECT request_hash FROM am_model_cache WHERE repository_id=? ORDER BY created_at DESC LIMIT -1 OFFSET 500)",params![job.repository,job.repository])?;
            // A cached answer is only as current as the principal that produced it.
            crate::invalidation::record_dependency(
                db,
                &job.workspace,
                (
                    crate::invalidation::ANSWER,
                    &format!("{}:{}", job.repository, request_hash),
                ),
                (crate::invalidation::PRINCIPAL, &job.principal_hash),
            )?;
            Ok(())
        })?;
        Ok(value)
    }
    pub async fn embeddings(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
    ) -> Result<Vec<Vec<f64>>> {
        self.embeddings_for(job, profile, texts, false).await
    }
    /// Embed retrieval questions; local models apply their manifest query prefix.
    pub async fn query_embeddings(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
    ) -> Result<Vec<Vec<f64>>> {
        self.embeddings_for(job, profile, texts, true).await
    }
    async fn embeddings_for(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
        query: bool,
    ) -> Result<Vec<Vec<f64>>> {
        let input = texts
            .iter()
            .map(|t| graphicle_memory::text::count(t))
            .sum::<usize>();
        let credential = profile
            .key_env
            .as_deref()
            .map(|reference| self.store.resolve_credential(&job.workspace, reference))
            .transpose()?;
        let call_id = id();
        let mut request = json!({"model":model::key(profile),"input":texts,"scope":job.repository});
        if query {
            request["role"] = json!("query");
        }
        let request_hash = digest(&request);
        guarded(&self.store, job, |db, _, _| {
            renew(db, job)?;
            let totals=one(db,"SELECT count(*) AS calls,coalesce(sum(input_reserved),0) AS input FROM am_call_reservations WHERE job_id=?",[&job.id])?.unwrap();
            if totals["calls"].as_u64().unwrap_or(0)
                >= job.payload["max_calls"].as_u64().unwrap_or(4)
                || totals["input"].as_u64().unwrap_or(0) + input as u64
                    > job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
            {
                return Err(Error::bad("Embedding budget exhausted"));
            }
            db.execute("INSERT INTO am_call_reservations(id,job_id,request_hash,state,input_reserved,output_reserved,created_at) VALUES(?,?,?,'started',?,0,?)",params![call_id,job.id,request_hash,input,now()])?;
            db.execute(
                "INSERT INTO am_model_calls VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    call_id,
                    job.repository,
                    job.id,
                    profile.protocol,
                    profile.model,
                    request_hash,
                    input,
                    0,
                    0,
                    0,
                    now()
                ],
            )?;
            Ok(())
        })?;
        let start = std::time::Instant::now();
        let result =
            model::embed_role(&self.client, profile, texts, credential.as_deref(), query).await;
        self.store.transaction(|db| {
            match &result {Ok((_,tokens,reported))=>{
                db.execute("UPDATE am_call_reservations SET state='finished',input_reserved=?,usage_reported=?,finished_at=? WHERE id=?",params![tokens,reported,now(),call_id])?;
                db.execute("UPDATE am_model_calls SET input_tokens=?,elapsed_ms=? WHERE id=?",params![tokens,start.elapsed().as_millis() as u64,call_id])?;
            },Err(error)=>{db.execute("UPDATE am_call_reservations SET state='unknown_usage',error=?,finished_at=? WHERE id=?",params![error.message,now(),call_id])?;}}Ok(())
        })?;
        let (vectors, _, _) = result?;
        guarded_read(&self.store, job, |_, _, _| Ok(()))?;
        Ok(vectors)
    }
}
/// How a reply that fails its output schema is reported (see `validate_output`).
pub const CONTRACT_MISMATCH: &str = "Model output does not match the workflow contract";
fn validate_output(schema: &Value, value: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(&model::strict_schema(schema))
        .map_err(|_| Error::bad("Invalid workflow output schema"))?;
    if let Some(error) = validator.iter_errors(value).next() {
        return Err(Error::bad(format!(
            "{CONTRACT_MISMATCH} at {} (schema {}); no memory was committed",
            error.instance_path, error.schema_path
        )));
    }
    Ok(())
}
