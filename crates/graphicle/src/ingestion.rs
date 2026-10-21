//! G08 · Bounded asynchronous ingestion.
//!
//! The AST/projection phase completes inside `sync.commit` (it only enqueues
//! the `project` job); everything derived — embeddings, extraction, observer
//! notes, an edition's semantic overlay — runs from `am_jobs` with:
//!
//! * `dedupe_by_repository`: a job of the same kind and repository with an
//!   identical `input_hash` is replayed, never duplicated (`UNIQUE
//!   (repository_id,kind,input_hash)` plus the replay branch in `enqueue`);
//! * `serialization_key = repository_id`: jobs run one at a time, except
//!   `observer_notes` jobs which both explicitly set `parallel_sources: true`
//!   and have disjoint `include` prefixes (`Store::lease_job`). Each retains its
//!   own budget, lease and source/access checks; global admission bounds apply;
//! * retry ≤ 3 with exponential backoff on `not_before` ([`backoff`]);
//! * bounds: concurrent CPU-bound jobs, concurrent model-bound jobs, and
//!   queued payload bytes per repository. An enqueue that would exceed the
//!   byte bound is **rejected** with a clear error rather than dropping older
//!   work: a silently dropped job would leave a caller waiting on an id that
//!   never completes, while a rejection is visible and retryable.
//!
//! Interactive operations (`context`, `search`, `impact`, ...) are never
//! queued; `Store::enqueue` refuses them explicitly.
use crate::{Error, Result, model};
use rusqlite::Connection;
use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default per-repository bound on queued (`queued`/`retry`) payload bytes.
pub const DEFAULT_QUEUED_BYTES: usize = 8 * 1024 * 1024;
/// Smallest configurable byte bound: one maximal workflow payload.
pub const MIN_QUEUED_BYTES: usize = 65536;
/// Default concurrent model-bound jobs.
pub const DEFAULT_MODEL_JOBS: usize = 2;
/// First retry delay; doubles per attempt up to [`MAX_BACKOFF_SECONDS`].
pub const BASE_BACKOFF_SECONDS: i64 = 30;
pub const MAX_BACKOFF_SECONDS: i64 = 30 * 60;

/// Operations answered inline under the caller's transaction; never jobs.
pub const INTERACTIVE: &[&str] = &["context", "search", "impact", "status", "read", "source"];

/// Kinds whose cost is dominated by local CPU (extraction, local inference)
/// rather than remote provider latency. Installed extensions add the kinds
/// they declare CPU-bound (`extension::JobKind::cpu`).
pub const CPU_BOUND: &[&str] = &["project", "link", "embed", "semantic_overlay"];

pub fn interactive(kind: &str) -> bool {
    INTERACTIVE.contains(&kind)
}
pub fn cpu_bound(kind: &str) -> bool {
    CPU_BOUND.contains(&kind)
        || crate::extension::installed()
            .job_kind(kind)
            .is_some_and(|declared| declared.cpu)
}
/// Every non-source, non-projection job may call a model.
pub fn model_bound(kind: &str) -> bool {
    !crate::jobs::source_task(kind) && kind != "project" && kind != "link"
}

pub fn default_cpu_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub max_cpu_jobs: usize,
    pub max_model_jobs: usize,
    pub max_queued_bytes_per_repository: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_cpu_jobs: default_cpu_jobs(),
            max_model_jobs: DEFAULT_MODEL_JOBS,
            max_queued_bytes_per_repository: DEFAULT_QUEUED_BYTES,
        }
    }
}

impl Bounds {
    pub fn from_config(config: &model::Config) -> Self {
        let defaults = Self::default();
        Self {
            max_cpu_jobs: config.max_cpu_jobs.unwrap_or(defaults.max_cpu_jobs).max(1),
            max_model_jobs: config
                .max_model_jobs
                .unwrap_or(defaults.max_model_jobs)
                .max(1),
            max_queued_bytes_per_repository: config
                .max_queued_bytes_per_repository
                .unwrap_or(defaults.max_queued_bytes_per_repository)
                .max(MIN_QUEUED_BYTES),
        }
    }
}

/// Payload bytes of jobs waiting to run for one repository.
pub fn queued_bytes(db: &Connection, repository: &str) -> Result<usize> {
    let total: i64 = db.query_row(
        "SELECT coalesce(sum(length(payload)),0) FROM am_jobs WHERE repository_id=? AND state IN ('queued','retry')",
        [repository],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(total).unwrap_or(usize::MAX))
}

/// Reject an enqueue whose payload would push the repository's queued bytes
/// over `bound`. Replays never reach this check.
pub fn check_queue_bytes(
    db: &Connection,
    repository: &str,
    incoming: usize,
    bound: usize,
) -> Result<()> {
    let queued = queued_bytes(db, repository)?;
    if queued.saturating_add(incoming) > bound {
        return Err(Error::bad(format!(
            "Repository job queue is full: {queued} bytes queued + {incoming} bytes requested exceed the {bound}-byte bound; wait for queued work to finish or cancel it"
        )));
    }
    Ok(())
}

/// Delay before attempt `attempt + 1` may start: 30 s, 60 s, 120 s, ... capped.
pub fn backoff(attempt: u64) -> chrono::Duration {
    let exponent = attempt.saturating_sub(1).min(16) as u32;
    let seconds = BASE_BACKOFF_SECONDS
        .saturating_mul(1i64 << exponent)
        .min(MAX_BACKOFF_SECONDS);
    chrono::Duration::seconds(seconds)
}

pub fn backoff_stamp(attempt: u64) -> String {
    (chrono::Utc::now() + backoff(attempt)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Process-wide admission limiter for CPU- and model-bound work.
#[derive(Clone)]
pub struct Limiter {
    cpu: Arc<Semaphore>,
    model: Arc<Semaphore>,
}

/// Permits a worker holds while running one job. Dropping releases them.
#[derive(Default)]
pub struct Admission {
    cpu: Option<OwnedSemaphorePermit>,
    model: Option<OwnedSemaphorePermit>,
}

impl Admission {
    /// Keep only the permits `kind` needs; the rest go back immediately.
    pub fn retain_for(&mut self, kind: &str) {
        if !cpu_bound(kind) {
            self.cpu = None;
        }
        if !model_bound(kind) {
            self.model = None;
        }
    }
    pub fn holds_cpu(&self) -> bool {
        self.cpu.is_some()
    }
    pub fn holds_model(&self) -> bool {
        self.model.is_some()
    }
}

impl Limiter {
    pub fn new(bounds: &Bounds) -> Self {
        Self {
            cpu: Arc::new(Semaphore::new(bounds.max_cpu_jobs.max(1))),
            model: Arc::new(Semaphore::new(bounds.max_model_jobs.max(1))),
        }
    }
    /// The limiter every worker in this process shares, sized by the first
    /// bounds seen (workers share one configuration).
    pub fn global(bounds: &Bounds) -> &'static Limiter {
        static GLOBAL: OnceLock<Limiter> = OnceLock::new();
        GLOBAL.get_or_init(|| Limiter::new(bounds))
    }
    /// Try to take one CPU and one model permit without waiting. Returns the
    /// permits obtained and the job kinds a worker must not lease without the
    /// missing ones. A lease is only taken for admissible work, so a job never
    /// burns its lease waiting on a permit.
    pub fn try_admit(&self) -> (Admission, Vec<&'static str>) {
        let take = |semaphore: &Arc<Semaphore>| Arc::clone(semaphore).try_acquire_owned().ok();
        let admission = Admission {
            cpu: take(&self.cpu),
            model: take(&self.model),
        };
        let mut excluded = Vec::new();
        if admission.cpu.is_none() {
            excluded.extend(CPU_BOUND.iter().copied());
            excluded.extend(crate::extension::installed().cpu_bound_kinds());
        }
        if admission.model.is_none() {
            excluded.extend(
                [
                    "extract",
                    "refresh",
                    "observer_notes",
                    "reflect",
                    "embed",
                    "recall",
                    "basic",
                    "semantic_overlay",
                ]
                .iter()
                .copied(),
            );
        }
        excluded.sort_unstable();
        excluded.dedup();
        (admission, excluded)
    }
    /// Wait until one CPU and one model permit are both held. Used when ready
    /// work exists but was excluded by `try_admit`: the worker applies
    /// backpressure instead of returning idle while a queue is non-empty.
    pub async fn admit(&self) -> Admission {
        let cpu = Arc::clone(&self.cpu).acquire_owned().await.ok();
        let model = Arc::clone(&self.model).acquire_owned().await.ok();
        Admission { cpu, model }
    }
    pub fn available_cpu(&self) -> usize {
        self.cpu.available_permits()
    }
    pub fn available_model(&self) -> usize {
        self.model.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0).num_seconds(), 30);
        assert_eq!(backoff(1).num_seconds(), 30);
        assert_eq!(backoff(2).num_seconds(), 60);
        assert_eq!(backoff(3).num_seconds(), 120);
        assert_eq!(backoff(40).num_seconds(), MAX_BACKOFF_SECONDS);
    }

    #[test]
    fn bounds_floor_configuration() {
        let config = model::Config {
            max_cpu_jobs: Some(0),
            max_model_jobs: Some(0),
            max_queued_bytes_per_repository: Some(1),
            ..Default::default()
        };
        let bounds = Bounds::from_config(&config);
        assert_eq!(bounds.max_cpu_jobs, 1);
        assert_eq!(bounds.max_model_jobs, 1);
        assert_eq!(bounds.max_queued_bytes_per_repository, MIN_QUEUED_BYTES);
        assert!(Bounds::default().max_cpu_jobs >= 1);
    }

    #[test]
    fn limiter_excludes_kinds_without_permits() {
        let limiter = Limiter::new(&Bounds {
            max_cpu_jobs: 1,
            max_model_jobs: 1,
            max_queued_bytes_per_repository: DEFAULT_QUEUED_BYTES,
        });
        let (mut first, excluded) = limiter.try_admit();
        assert!(excluded.is_empty());
        let (second, excluded) = limiter.try_admit();
        assert!(!second.holds_cpu() && !second.holds_model());
        assert!(excluded.contains(&"project") && excluded.contains(&"reflect"));
        assert!(!excluded.contains(&"connector"));
        first.retain_for("project");
        assert!(first.holds_cpu() && !first.holds_model());
        assert_eq!(limiter.available_model(), 1);
        drop(first);
        assert_eq!(limiter.available_cpu(), 1);
    }

    #[test]
    fn interactive_kinds_are_never_jobs() {
        assert!(interactive("context") && interactive("search") && interactive("impact"));
        assert!(!interactive("project") && !interactive("embed"));
        assert!(cpu_bound("project") && !model_bound("project"));
        assert!(model_bound("embed") && !model_bound("connector"));
    }
}
