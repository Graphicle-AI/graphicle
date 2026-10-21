//! What Graphicle serves, metered: the tokens of the results it returns to a
//! credential or an agent session.
//!
//! **Counted** ([`meter`]):
//! - every MCP tool result (`tools/call`, over stdio and HTTP, on project and
//!   workspace connections), per tool: the core tools with the graph engine's
//!   navigation tools, the learning, work-state and pull-request tools, and
//!   the installed extensions' tools;
//! - over HTTP and through `graphicle call` and `graphicle context`, per
//!   operation: every operation a published MCP tool runs, and the other
//!   reads that return a project's source, memory, evidence or graph
//!   ([`TEXT_READS`], and an installed extension's `metered_operations`).
//!   Over HTTP that covers the command routes and the repository routes that
//!   serve such an operation (the graph and impact routes, [`serve_route`]);
//! - every MCP resource read (`resources/read`: the code-graph report and
//!   statistics a project connection lists);
//! - the session-start context a capture hook gives a coding agent.
//!
//! A call that names no project but whose result names projects (a
//! workspace connection's recall over every project it may read, or a
//! workspace-wide report such as an installed extension's digest) is counted
//! on those projects ([`shares`]): its tokens split by the bytes served from
//! each, and the call once, on the largest share.
//!
//! **Not counted:** the other operations and routes, which return settings,
//! counts or listings and no project text, or change state without a tool
//! (for example `observer.status`, `retention.preview`, `connector.list`,
//! `model.status`, `learning.review`, the project overview route and
//! workspace administration); the other local commands, which print to the
//! operator on the server (`graphicle status`, `graphicle work-state`,
//! `graphicle sessions`, `graphicle ask`); and a result that names no
//! project, such as `list_projects` (it is still admitted, below).
//!
//! A result's tokens are counted with cl100k_base
//! (`graphicle_memory::text::count`) on the text served: an operation's
//! result as compact JSON over HTTP, an MCP tool's text content, what the
//! command line prints (its readable view on a terminal, else indented JSON),
//! or the start context. They are the tokens Graphicle served, not the
//! agent's bill: Graphicle does not see or count the agent's own model calls,
//! its prompts, other tools and output are not counted here, and its model
//! may count tokens differently.
//!
//! **Kept.** Counters only, in `am_served_usage`: per UTC day, project,
//! credential (`owner` for the owner capability), agent session, channel
//! ([`Channel`]) and tool or operation, the calls and their tokens. No
//! question, result, source id or client session id is kept. Rows go with
//! their project. They are kept 25 months, and rows of a session older than
//! 90 days are folded into rows without one. `deletion.state` lists the
//! table as history.
//!
//! **Agent sessions.** An MCP stdio connection (`graphicle mcp`), an MCP HTTP
//! session (the `Mcp-Session-Id` the server issues and signs at `initialize`;
//! an id it did not sign is labelled [`UNVERIFIED_SESSION`]), or the coding
//! agent's own session for its start context. A label names the client and
//! a random id, or the first 12 hex digits of the SHA-256 of the client's
//! own session id ([`session_label`]). One credential keeps at most
//! [`SESSIONS_PER_DAY`] session labels a UTC day: the calls of further
//! sessions that day are counted under one shared label,
//! [`OTHER_SESSIONS`], so a client that opens session after session cannot
//! grow the counters without bound.
//!
//! **Nothing on the request path.** Metering adds no write under the store
//! lock to a call. A result's tokens are counted on a thread of its own
//! after it is served ([`record_text`]), and counts gather in this process.
//! They reach the store in one write transaction per batch: from a
//! background thread [`FLUSH_DELAY`] after the first unrecorded count, before
//! the owner's usage report once the owner is authorized (waiting at most
//! [`REPORT_WAIT`], [`flush_briefly`]), and when the program ends
//! ([`flush_all`]). A process that is killed loses its unrecorded counts.
//!
//! **Admission.** Every call of a workspace credential goes through one
//! admission ([`admit`]), in this order: the installed extensions authorize
//! it (`Extension::authorize`: they may refuse it, narrow its arguments or
//! set the most tokens one result may hold); then, for a metered call, they
//! may refuse it as over a quota (`Extension::served_allowed`), with this
//! process's unrecorded tokens in view ([`Served::unrecorded_tokens`]); then
//! the call runs; then its result is served ([`Admission::serve`]): a read's
//! result must fit the one per-call cap (`extension::within_limit`, 413), and
//! a metered result is counted. What is a read is decided by [`meter`]
//! ([`Meter::Read`]); a write's result is never refused once the write is
//! saved. The caller is resolved by authentication only
//! (`store::authenticate_hash`), so a credential is checked and counted
//! whichever permissions its role holds. A refused HTTP call keeps the
//! refusal's status, and a 429 carries `Retry-After` until the next UTC month
//! ([`refusal`]). The owner capability is never asked. The local command line
//! and capture hooks are counted, never refused for a quota. Core installs no
//! extension, so it refuses and caps nothing.
use crate::{
    Error, Result,
    extension::{Access, AccessRequest},
    store::{Principal, Store, rows},
};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// Core's reads that no published MCP tool runs and that return a project's
/// source, memory, evidence or graph: metered over HTTP and the command line
/// beside the operations the tools run ([`meter`]).
pub const TEXT_READS: &[&str] = &[
    "claim.history",
    "document.original",
    "document.versions",
    "graph.cross_edges",
    "graph.doc_links",
    "graph.neo4j.search",
    "graph.query",
    "graph.source",
    "history",
    "memory.export",
    "memory.template",
    "observer.context",
];
/// Operations that run another one under a second name: metered as it is.
const ALIASES: &[(&str, &str)] = &[("evidence.read", "read"), ("runtime.status", "status")];
/// Operations a tool annotated read-only runs that save state all the same
/// (`graph_link` stores the cross-repository edges): metered as writes, so
/// their result is never refused after it is saved.
const WRITES: &[&str] = &["graph.link"];
/// The label of every MCP HTTP session id this server did not sign.
pub const UNVERIFIED_SESSION: &str = "mcp:unverified";
/// The most agent-session labels one credential keeps in a UTC day.
pub const SESSIONS_PER_DAY: i64 = 200;
/// The label the calls of a credential's further sessions in a day share
/// ([`SESSIONS_PER_DAY`]). Client labels always hold a colon, so it names
/// no client's session.
pub const OTHER_SESSIONS: &str = "other-sessions";
/// How long after its first unrecorded count a batch is written.
pub const FLUSH_DELAY: Duration = Duration::from_secs(5);
/// The longest a usage report waits for the write lock to record this
/// process's counts first ([`flush_briefly`]).
pub const REPORT_WAIT: Duration = Duration::from_secs(1);
/// Days a session's own rows are kept before they are folded into rows
/// without a session.
pub const SESSION_DAYS: i64 = 90;
/// Days rows are kept: 25 months, as the request counters.
pub const KEEP_DAYS: i64 = 760;
/// How long a flush waits for a batch another thread is writing, and for
/// results still being counted.
const WAIT: Duration = Duration::from_secs(15);
/// Results counted at once on threads of their own; beyond this the caller
/// counts.
const COUNTING: usize = 16;
/// Failed background writes before a batch is given up (about five minutes).
const ATTEMPTS: u32 = 60;
/// The longest tool or operation name a counter keeps.
const KIND_MAX: usize = 100;

/// Where a result was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    /// An MCP tool result, over stdio or HTTP.
    Mcp,
    /// An operation over HTTP from an API client.
    Http,
    /// An operation over HTTP from the web app.
    Web,
    /// The local command line (`graphicle context`, `graphicle call`).
    Cli,
    /// A capture hook's session-start context.
    Hook,
}

impl Channel {
    /// The name reports and the store use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Http => "http",
            Self::Web => "web",
            Self::Cli => "cli",
            Self::Hook => "hook",
        }
    }
    /// An HTTP request's channel: `web` when it carries the web app's
    /// `X-Graphicle-Request: 1`, else `http`. A label for reports only:
    /// nothing is limited or exempted by it.
    pub fn of(headers: &HeaderMap) -> Self {
        if headers
            .get("x-graphicle-request")
            .is_some_and(|value| value == "1")
        {
            Self::Web
        } else {
            Self::Http
        }
    }
}

/// Where one served result went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub workspace: String,
    /// The project the result came from.
    pub repository: String,
    /// `owner` for the owner capability, else the credential's id.
    pub credential: String,
    /// The agent session's label ([`session_label`]); empty for none.
    pub session: String,
    pub channel: Channel,
    /// The MCP tool, the operation, or `session_start`.
    pub kind: String,
}

/// A metered call before it runs, as `Extension::served_allowed` sees it.
#[derive(Clone, Copy, Debug)]
pub struct Served<'a> {
    /// The project; empty for a call that names none (admitted, never
    /// counted).
    pub repository: &'a str,
    pub session: &'a str,
    pub channel: Channel,
    /// The MCP tool, or the HTTP operation ([`meter`]; [`tools_for`] names
    /// the tools that run it).
    pub kind: &'a str,
    /// Tokens this process served the credential this UTC month that the
    /// store does not hold yet.
    pub unrecorded_tokens: u64,
}

/// How a metered operation or tool is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meter {
    /// A read: the per-call cap an installed extension sets applies to its
    /// result ([`Admission::serve`]).
    Read,
    /// A write a published tool runs (its tool is not read-only): counted
    /// and admitted, never refused after it ran.
    Write,
}

/// Every metered operation, with how it is metered and the published tools
/// that run it.
type Meters = BTreeMap<&'static str, (Meter, Vec<String>)>;

fn meters() -> &'static Meters {
    // The installed extensions are fixed once installed; before that (and
    // in Core) the core tools alone.
    static CORE: OnceLock<Meters> = OnceLock::new();
    static EDITION: OnceLock<Meters> = OnceLock::new();
    let extensions = crate::extension::installed();
    let cell = if extensions.is_empty() {
        &CORE
    } else {
        &EDITION
    };
    cell.get_or_init(|| {
        let mut meters = Meters::new();
        for tool in crate::api::core_mcp_tools(true, true)
            .into_iter()
            .chain(extensions.mcp_tools(true, true))
        {
            let meter = if tool.spec["annotations"]["readOnlyHint"] == true {
                Meter::Read
            } else {
                Meter::Write
            };
            let entry = meters.entry(tool.operation).or_insert((meter, Vec::new()));
            if meter == Meter::Write {
                entry.0 = Meter::Write;
            }
            entry.1.push(tool.name().to_string());
        }
        for operation in TEXT_READS
            .iter()
            .copied()
            .chain(extensions.metered_operations())
        {
            meters.entry(operation).or_insert((Meter::Read, Vec::new()));
        }
        for operation in WRITES {
            if let Some(entry) = meters.get_mut(operation) {
                entry.0 = Meter::Write;
            }
        }
        for &(alias, operation) in ALIASES {
            if let Some(entry) = meters.get(operation).cloned() {
                meters.insert(alias, entry);
            }
        }
        meters
    })
}

/// Whether `operation` is metered over HTTP and the command line, and how:
/// every operation a published MCP tool runs (a [`Meter::Write`] when a tool
/// that runs it is not read-only, or the operation saves state anyway,
/// [`WRITES`]), and the other reads that return a project's text
/// ([`TEXT_READS`], an installed extension's `metered_operations`). `None` for
/// any other operation. An MCP tool's result is capped only when its
/// operation is a [`Meter::Read`].
pub fn meter(operation: &str) -> Option<Meter> {
    meters().get(operation).map(|(meter, _)| *meter)
}

/// The published MCP tools (the core tools, learning and work-state tools
/// included, and the installed extensions' tools) that run `operation`; none
/// for a metered read no tool runs, or an operation that is not metered.
pub fn tools_for(operation: &str) -> Vec<String> {
    meters()
        .get(operation)
        .map(|(_, tools)| tools.clone())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    workspace: String,
    repository: String,
    day: String,
    credential: String,
    session: String,
    channel: Channel,
    kind: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Count {
    calls: u64,
    tokens: u64,
}

type Counts = BTreeMap<Key, Count>;

fn merge(into: &mut Counts, from: Counts) {
    for (key, count) in from {
        let entry = into.entry(key).or_default();
        entry.calls = entry.calls.saturating_add(count.calls);
        entry.tokens = entry.tokens.saturating_add(count.tokens);
    }
}

/// One store's counts in this process.
struct Pending {
    store: Store,
    counts: Counts,
    /// Batches being written, by number.
    writing: BTreeMap<u64, Counts>,
    /// Results being counted on threads of their own.
    counting: usize,
    /// A background flush is scheduled.
    scheduled: bool,
}

impl Pending {
    fn of<'a>(registry: &'a mut Registry, store: &Store) -> &'a mut Pending {
        registry
            .stores
            .entry(place(store))
            .or_insert_with(|| Pending {
                store: store.clone(),
                counts: Counts::new(),
                writing: BTreeMap::new(),
                counting: 0,
                scheduled: false,
            })
    }
    /// Nothing unrecorded, being written or being counted.
    fn idle(&self) -> bool {
        self.counts.is_empty() && self.writing.is_empty() && self.counting == 0
    }
}

struct Registry {
    next: u64,
    stores: BTreeMap<PathBuf, Pending>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    next: 0,
    stores: BTreeMap::new(),
});
/// Signalled when a batch has been written or given back.
static WRITTEN: Condvar = Condvar::new();

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn place(store: &Store) -> PathBuf {
    store.database.as_ref().clone()
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The tokens of a served text (cl100k_base).
pub fn tokens(text: &str) -> u64 {
    graphicle_memory::text::count(text) as u64
}

/// Count one served result of `tokens` tokens. No I/O: the count reaches the
/// store in a later batch.
pub fn record(store: &Store, call: &Call, tokens: u64) {
    add(store, call, 1, tokens);
}

/// Add `calls` calls and `tokens` tokens to `call`'s counter.
fn add(store: &Store, call: &Call, calls: u64, tokens: u64) {
    if call.workspace.is_empty() || call.repository.is_empty() {
        return;
    }
    let key = Key {
        workspace: call.workspace.clone(),
        repository: call.repository.clone(),
        day: today(),
        credential: call.credential.clone(),
        session: call.session.clone(),
        channel: call.channel,
        kind: call.kind.chars().take(KIND_MAX).collect(),
    };
    let schedule = {
        let mut registry = registry();
        let pending = Pending::of(&mut registry, store);
        let count = pending.counts.entry(key).or_default();
        count.calls = count.calls.saturating_add(calls);
        count.tokens = count.tokens.saturating_add(tokens);
        !std::mem::replace(&mut pending.scheduled, true)
    };
    if schedule {
        schedule_flush(store.clone());
    }
}

/// The projects a result that names no project serves, with the bytes it
/// serves from each: the sources of a recall packet (each names its project
/// in `repository_id`, as an MCP connection records them) and the sections
/// of a workspace-wide report (`repositories`, each with its
/// `repository_id`). Empty for a result that names none, such as
/// `list_projects`.
pub fn shares(result: &Value) -> Vec<(String, u64)> {
    let mut bytes: BTreeMap<String, u64> = BTreeMap::new();
    for item in ["sources", "repositories"]
        .into_iter()
        .flat_map(|list| result[list].as_array().into_iter().flatten())
    {
        if let Some(project) = item["repository_id"].as_str().filter(|id| !id.is_empty()) {
            let size = item.to_string().len() as u64;
            let entry = bytes.entry(project.to_string()).or_default();
            *entry = entry.saturating_add(size);
        }
    }
    bytes.into_iter().collect()
}

/// Count `tokens` of a call that names no project on the projects its result
/// serves ([`shares`]): the tokens split by bytes, the call once, on the
/// largest share. Nothing is counted without a share.
fn split(store: &Store, call: &Call, tokens: u64, shares: &[(String, u64)]) {
    let total: u64 = shares
        .iter()
        .map(|(_, bytes)| *bytes)
        .fold(0, u64::saturating_add);
    let Some(largest) = shares
        .iter()
        .enumerate()
        .max_by(|(a, (_, x)), (b, (_, y))| x.cmp(y).then(b.cmp(a)))
        .map(|(index, _)| index)
    else {
        return;
    };
    let mut given = 0u64;
    let portions: Vec<u64> = shares
        .iter()
        .map(|(_, bytes)| {
            let portion = if total == 0 {
                0
            } else {
                (u128::from(tokens) * u128::from(*bytes) / u128::from(total)) as u64
            };
            given = given.saturating_add(portion);
            portion
        })
        .collect();
    for (index, ((project, _), portion)) in shares.iter().zip(portions).enumerate() {
        // What rounding left over goes with the call, to the largest share.
        let (calls, tokens) = if index == largest {
            (1, portion.saturating_add(tokens.saturating_sub(given)))
        } else {
            (0, portion)
        };
        let mut on = call.clone();
        on.repository = project.clone();
        add(store, &on, calls, tokens);
    }
}

/// Count a served `text` on a thread of its own, then [`record`] it, so the
/// caller serves at once; with many results being counted already, count it
/// here.
pub fn record_text(store: &Store, call: Call, text: String) {
    record_served(store, call, text, Vec::new());
}

/// [`record_text`], for a call that names no project counted on the
/// projects its result serves ([`shares`], [`split`]).
pub fn record_served(store: &Store, call: Call, text: String, shares: Vec<(String, u64)>) {
    if call.repository.is_empty() && shares.is_empty() {
        return;
    }
    let count = move |store: &Store, call: &Call, text: &str| {
        if call.repository.is_empty() {
            split(store, call, tokens(text), &shares);
        } else {
            record(store, call, tokens(text));
        }
    };
    let threaded = {
        let mut registry = registry();
        let pending = Pending::of(&mut registry, store);
        let free = pending.counting < COUNTING;
        if free {
            pending.counting += 1;
        }
        free
    };
    if !threaded {
        count(store, &call, &text);
        return;
    }
    let worker = store.clone();
    let started = std::thread::Builder::new()
        .name("graphicle-metering".into())
        .spawn(move || {
            count(&worker, &call, &text);
            counted(&worker);
        });
    if started.is_err() {
        // No thread to count it: this one result goes uncounted.
        counted(store);
        eprintln!("graphicle: a served result was not counted (no thread)");
    }
}

/// A result being counted on its own thread is done.
fn counted(store: &Store) {
    if let Some(pending) = registry().stores.get_mut(&place(store)) {
        pending.counting = pending.counting.saturating_sub(1);
    }
    WRITTEN.notify_all();
}

/// Write `store`'s counts [`FLUSH_DELAY`] from now, and again while more
/// arrive; a batch that keeps failing is given up after about five minutes,
/// at once when the store is gone.
fn schedule_flush(store: Store) {
    let worker = store.clone();
    let started = std::thread::Builder::new()
        .name("graphicle-metering".into())
        .spawn(move || {
            let store = worker;
            let mut failures = 0;
            loop {
                std::thread::sleep(FLUSH_DELAY);
                match flush(&store) {
                    Ok(_) => failures = 0,
                    Err(error) => {
                        failures += 1;
                        let gone = !store.database.exists();
                        if gone || failures >= ATTEMPTS {
                            let lost = discard(&store);
                            if !gone {
                                eprintln!(
                                    "graphicle: {lost} served-token counts were not recorded: {}",
                                    error.message
                                );
                            }
                            failures = 0;
                        }
                    }
                }
                let mut registry = registry();
                let key = place(&store);
                let Some(pending) = registry.stores.get_mut(&key) else {
                    return;
                };
                if pending.counts.is_empty() {
                    pending.scheduled = false;
                    if pending.idle() {
                        registry.stores.remove(&key);
                    }
                    return;
                }
            }
        });
    if started.is_err()
        && let Some(pending) = registry().stores.get_mut(&place(&store))
    {
        // No thread: the next count, a report or the program's end writes.
        pending.scheduled = false;
    }
}

/// Drop `store`'s unrecorded counts; how many counters were dropped.
fn discard(store: &Store) -> usize {
    registry()
        .stores
        .get_mut(&place(store))
        .map(|pending| std::mem::take(&mut pending.counts).len())
        .unwrap_or(0)
}

/// How long a flush waits for each thing it may wait on.
#[derive(Clone, Copy, Debug)]
struct Waits {
    /// Results still being counted on their own threads: this process's own
    /// work.
    counting: Duration,
    /// Batches other threads are writing.
    batches: Duration,
    /// The write lock; `None` for the store's usual 10 seconds.
    lock: Option<Duration>,
}

/// Write `store`'s unrecorded counts now, after the results still being
/// counted and any batch another thread is writing (waiting at most [`WAIT`]
/// for each); how many counters were written.
pub fn flush(store: &Store) -> Result<usize> {
    flush_waiting(
        store,
        Waits {
            counting: WAIT,
            batches: WAIT,
            lock: None,
        },
    )
}

/// Before a usage report, once its caller is authorized: write this
/// process's unrecorded counts without waiting for results still being
/// counted or for batches other threads are writing, and waiting at most
/// [`REPORT_WAIT`] for the write lock. Counts left over stay unrecorded, and
/// the report gives them as `unrecorded_tokens` ([`unrecorded_in`]); results
/// still being counted are in neither.
pub fn flush_briefly(store: &Store) -> Result<usize> {
    flush_waiting(
        store,
        Waits {
            counting: Duration::ZERO,
            batches: Duration::ZERO,
            lock: Some(REPORT_WAIT),
        },
    )
}

/// [`flush`] within `waits`.
fn flush_waiting(store: &Store, waits: Waits) -> Result<usize> {
    let key = place(store);
    let (number, batch) = {
        let mut registry = registry();
        let started = Instant::now();
        loop {
            let (counting, writing) = registry.stores.get(&key).map_or((false, false), |pending| {
                (pending.counting > 0, !pending.writing.is_empty())
            });
            // The longest bound of what is still pending.
            let limit = [(counting, waits.counting), (writing, waits.batches)]
                .into_iter()
                .filter(|(pending, _)| *pending)
                .map(|(_, bound)| bound)
                .max();
            let Some(left) = limit.map(|limit| limit.saturating_sub(started.elapsed())) else {
                break;
            };
            if left.is_zero() {
                break;
            }
            registry = WRITTEN
                .wait_timeout(registry, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        registry.next += 1;
        let number = registry.next;
        let Some(pending) = registry.stores.get_mut(&key) else {
            return Ok(0);
        };
        if pending.counts.is_empty() {
            return Ok(0);
        }
        let batch = std::mem::take(&mut pending.counts);
        pending.writing.insert(number, batch.clone());
        (number, batch)
    };
    let written = match waits.lock {
        None => store.transaction(|db| write(db, &batch)),
        Some(wait) => write_waiting(store, &batch, wait),
    };
    {
        let mut registry = registry();
        if let Some(pending) = registry.stores.get_mut(&key)
            && let Some(batch) = pending.writing.remove(&number)
            && written.is_err()
        {
            merge(&mut pending.counts, batch);
        }
    }
    WRITTEN.notify_all();
    written.map(|()| batch.len())
}

/// When the program ends: write every store's unrecorded counts, waiting at
/// most `wait` for each store's lock and other batches (and up to [`WAIT`]
/// for results still being counted). A failure is reported on standard
/// error.
pub fn flush_all(wait: Duration) {
    let stores: Vec<Store> = registry()
        .stores
        .values()
        .filter(|pending| !pending.idle())
        .map(|pending| pending.store.clone())
        .collect();
    for store in stores {
        let waits = Waits {
            counting: WAIT,
            batches: wait,
            lock: Some(wait),
        };
        if let Err(error) = flush_waiting(&store, waits) {
            eprintln!(
                "graphicle: served-token counts were not recorded: {}",
                error.message
            );
        }
    }
}

/// A write transaction that waits at most `wait` for the write lock.
fn write_waiting(store: &Store, batch: &Counts, wait: Duration) -> Result<()> {
    let mut db = store.connect()?;
    db.busy_timeout(wait)?;
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    write(&transaction, batch)?;
    transaction.commit()?;
    Ok(())
}

/// Add a batch to the stored counters; counts of a project deleted since are
/// dropped. Then the retention of the batch's workspaces.
fn write(db: &Connection, batch: &Counts) -> Result<()> {
    let mut insert = db.prepare(
        "INSERT INTO am_served_usage(repository_id,workspace_id,day,credential_id,session,channel,kind,calls,tokens)
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9 WHERE EXISTS(SELECT 1 FROM repo_projects WHERE id=?1 AND workspace_id=?2)
         ON CONFLICT(repository_id,day,credential_id,session,channel,kind)
         DO UPDATE SET calls=calls+excluded.calls,tokens=tokens+excluded.tokens",
    )?;
    let mut workspaces = BTreeSet::new();
    for (key, count) in batch {
        let session = day_session(db, key)?;
        insert.execute(params![
            key.repository,
            key.workspace,
            key.day,
            key.credential,
            session,
            key.channel.as_str(),
            key.kind,
            count.calls.min(i64::MAX as u64) as i64,
            count.tokens.min(i64::MAX as u64) as i64,
        ])?;
        workspaces.insert(key.workspace.as_str());
    }
    let now = chrono::Utc::now();
    let fold = (now - chrono::Duration::days(SESSION_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let cutoff = (now - chrono::Duration::days(KEEP_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    for workspace in workspaces {
        db.execute(
            "INSERT INTO am_served_usage(repository_id,workspace_id,day,credential_id,session,channel,kind,calls,tokens)
             SELECT repository_id,workspace_id,day,credential_id,'',channel,kind,sum(calls),sum(tokens)
             FROM am_served_usage WHERE workspace_id=?1 AND day<?2 AND session<>''
             GROUP BY repository_id,workspace_id,day,credential_id,channel,kind
             ON CONFLICT(repository_id,day,credential_id,session,channel,kind)
             DO UPDATE SET calls=calls+excluded.calls,tokens=tokens+excluded.tokens",
            params![workspace, fold],
        )?;
        db.execute(
            "DELETE FROM am_served_usage WHERE workspace_id=? AND day<? AND session<>''",
            params![workspace, fold],
        )?;
        db.execute(
            "DELETE FROM am_served_usage WHERE workspace_id=? AND day<?",
            params![workspace, cutoff],
        )?;
    }
    Ok(())
}

/// The session label a counter is written under: its own, unless its
/// credential already keeps [`SESSIONS_PER_DAY`] other labels that UTC day,
/// in which case [`OTHER_SESSIONS`].
fn day_session<'a>(db: &Connection, key: &'a Key) -> Result<&'a str> {
    if key.session.is_empty() || key.session == UNVERIFIED_SESSION {
        return Ok(key.session.as_str());
    }
    let (known, others): (bool, i64) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM am_served_usage WHERE workspace_id=?1 AND credential_id=?2 AND day=?3 AND session=?4),
         (SELECT count(DISTINCT session) FROM am_served_usage WHERE workspace_id=?1 AND credential_id=?2 AND day=?3
          AND session NOT IN ('',?5,?6))",
        params![
            key.workspace,
            key.credential,
            key.day,
            key.session,
            UNVERIFIED_SESSION,
            OTHER_SESSIONS
        ],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(if known || others < SESSIONS_PER_DAY {
        key.session.as_str()
    } else {
        OTHER_SESSIONS
    })
}

fn this_month() -> String {
    chrono::Utc::now().format("%Y-%m-").to_string()
}

/// Tokens this process served `credential` of `workspace` in the current
/// UTC month that the store does not hold yet (results still being counted
/// on their own threads are not in view).
pub fn unrecorded(store: &Store, workspace: &str, credential: &str) -> u64 {
    let month = this_month();
    unrecorded_where(store, |key| {
        key.workspace == workspace && key.credential == credential && key.day.starts_with(&month)
    })
}

/// [`unrecorded`] for every credential of `workspace`.
pub fn unrecorded_in(store: &Store, workspace: &str) -> u64 {
    let month = this_month();
    unrecorded_where(store, |key| {
        key.workspace == workspace && key.day.starts_with(&month)
    })
}

/// [`unrecorded_in`] for the UTC days `from` through `to` (`YYYY-MM-DD`).
pub fn unrecorded_between(store: &Store, workspace: &str, from: &str, to: &str) -> u64 {
    unrecorded_where(store, |key| {
        key.workspace == workspace && key.day.as_str() >= from && key.day.as_str() <= to
    })
}

fn unrecorded_where(store: &Store, keep: impl Fn(&Key) -> bool) -> u64 {
    let registry = registry();
    let Some(pending) = registry.stores.get(&place(store)) else {
        return 0;
    };
    pending
        .counts
        .iter()
        .chain(pending.writing.values().flatten())
        .filter(|(key, _)| keep(key))
        .map(|(_, count)| count.tokens)
        .fold(0, u64::saturating_add)
}

/// A client's name as a session label keeps it: lowercase ASCII letters,
/// digits, `.`, `_` and `-`, at most 40 characters; `agent` when nothing is
/// left.
pub fn client_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(40)
        .collect();
    if kept.is_empty() {
        "agent".into()
    } else {
        kept
    }
}

/// The label of a session a client names itself, such as a coding agent's
/// session for its start context: the client's name and the first 12 hex
/// digits of the SHA-256 of its session id, never the id.
pub fn session_label(client: &str, id: &str) -> String {
    format!(
        "{}:{}",
        client_name(client),
        &graphicle_memory::text::sha256(id.as_bytes())[..12]
    )
}

/// A new session label for `client`: its name and a random id.
pub fn new_session(client: &str) -> String {
    format!("{}:{}", client_name(client), &crate::id()[..12])
}

/// The key that signs `workspace`'s MCP HTTP session ids: derived from the
/// digest of its owner capability, which only the store holds, so a client
/// cannot make an id of its own pass.
fn session_key(store: &Store, workspace: &str) -> Option<ring::hmac::Key> {
    let digest: String = store
        .read(|db| {
            Ok(db.query_row(
                "SELECT capability_hash FROM workspaces WHERE id=?",
                [workspace],
                |r| r.get(0),
            )?)
        })
        .ok()?;
    Some(ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        format!("graphicle mcp session\n{digest}").as_bytes(),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn lower_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A new MCP HTTP session id for `client` in `workspace` (`Mcp-Session-Id`):
/// `<client>.<random>.<signature>`, visible ASCII as the protocol asks. `None`
/// when the workspace's key cannot be read.
pub fn issue_http_session(store: &Store, workspace: &str, client: &str) -> Option<String> {
    let key = session_key(store, workspace)?;
    let signed = format!("{}.{}", client_name(client), crate::id());
    let signature = ring::hmac::sign(&key, signed.as_bytes());
    Some(format!("{signed}.{}", hex(signature.as_ref())))
}

/// The label of an MCP HTTP session from its `Mcp-Session-Id`: an id this
/// server issued for `workspace` keeps its client's name and the first 12
/// digits of its random part; any other value, which a client could vary on
/// every call, is labelled [`UNVERIFIED_SESSION`].
pub fn http_session(store: &Store, workspace: &str, id: &str) -> String {
    let verified = id.rsplit_once('.').and_then(|(signed, signature)| {
        let (client, random) = signed.rsplit_once('.')?;
        if client_name(client) != client || !lower_hex(random, 32) || !lower_hex(signature, 64) {
            return None;
        }
        let signature: Vec<u8> = (0..signature.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&signature[at..at + 2], 16))
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        let key = session_key(store, workspace)?;
        ring::hmac::verify(&key, signed.as_bytes(), &signature).ok()?;
        Some(format!("{client}:{}", &random[..12]))
    });
    verified.unwrap_or_else(|| UNVERIFIED_SESSION.into())
}

/// The principal `token` authenticates in `workspace`, if any, by
/// authentication only (`store::authenticate_hash`): a credential is admitted
/// and counted whichever permissions its role holds.
pub fn principal(store: &Store, workspace: &str, token: &str) -> Option<Principal> {
    if token.is_empty() || token.len() > 512 {
        return None;
    }
    let hash = graphicle_memory::text::sha256(token.as_bytes());
    store
        .read(|db| crate::store::authenticate_hash(db, workspace, &hash))
        .ok()
}

/// One call to admit ([`admit`]).
#[derive(Debug)]
pub struct Ask<'a> {
    /// What the caller asks for, as `Extension::authorize` sees it: the
    /// operation, or the MCP resource read as `graph.navigate`.
    pub access: Access<'a>,
    /// The arguments of an operation, which `Extension::authorize` may narrow.
    pub arguments: Option<&'a Value>,
    /// Where its result goes; `credential` is filled in from the caller.
    pub call: Call,
    /// Metered: the quota is asked (`Extension::served_allowed`, never for the
    /// command line or capture hooks) and the result counted.
    pub metered: bool,
    /// A read: its result must fit the per-call cap ([`Admission::serve`]).
    pub read: bool,
}

/// What [`admit`] allowed: the arguments an installed extension narrowed, and
/// how the call's result is served ([`Admission::serve`]).
#[derive(Debug, Default)]
pub struct Admission {
    arguments: Option<Value>,
    /// The one per-call cap of a read's result: the lowest
    /// `AccessRequest::max_tokens` the installed extensions set.
    cap: Option<usize>,
    /// Where a metered result of an authenticated caller is counted.
    call: Option<Call>,
}

impl Admission {
    /// The arguments an installed extension narrowed, if it did: the call
    /// runs with these.
    pub fn take_arguments(&mut self) -> Option<Value> {
        self.arguments.take()
    }
    /// Nothing to check or count when the result is served.
    pub fn is_idle(&self) -> bool {
        self.cap.is_none() && self.call.is_none()
    }
    /// A metered call that names no project: it is counted on the projects
    /// its result serves ([`shares`]).
    pub fn needs_shares(&self) -> bool {
        self.call
            .as_ref()
            .is_some_and(|call| call.repository.is_empty())
    }
    /// Serve `text`, the text of the admitted call's result: the size check
    /// of a read (`extension::within_limit`, 413), then the count of a
    /// metered result ([`record_served`]; `shares` for a call that names no
    /// project).
    pub fn serve(self, store: &Store, text: String, shares: Vec<(String, u64)>) -> Result<()> {
        crate::extension::within_limit(self.cap, &text)?;
        if let Some(call) = self.call {
            record_served(store, call, text, shares);
        }
        Ok(())
    }
    /// [`Self::serve`] for a result served as compact JSON, serialized only
    /// when there is something to do.
    pub fn serve_value(self, store: &Store, result: &Value) -> Result<()> {
        if self.is_idle() {
            return Ok(());
        }
        let shares = if self.needs_shares() {
            shares(result)
        } else {
            Vec::new()
        };
        self.serve(store, result.to_string(), shares)
    }
}

/// The one admission of a call ([module documentation](self)): the installed
/// extensions authorize it (`Extension::authorize`), then, for a metered call
/// from anywhere but the command line and capture hooks, its quota
/// (`Extension::served_allowed`). The caller then runs it, with the narrowed
/// arguments ([`Admission::take_arguments`]), and serves its result through
/// [`Admission::serve`]. The owner capability is never asked, and its
/// metered results are counted. A caller that does not authenticate is
/// admitted with nothing to check or count: the call's own authorization
/// refuses it, as before. Core installs no extension, so it only counts.
pub fn admit(store: &Store, token: &str, ask: Ask<'_>) -> Result<Admission> {
    let extensions = crate::extension::installed();
    let Ask {
        access,
        arguments,
        mut call,
        metered,
        read,
    } = ask;
    if (!metered && extensions.is_empty()) || token.is_empty() || token.len() > 512 {
        return Ok(Admission::default());
    }
    let hash = graphicle_memory::text::sha256(token.as_bytes());
    store.read(|db| {
        let principal = match crate::store::authenticate_hash(db, &call.workspace, &hash) {
            Ok(principal) => principal,
            // Not a usable credential: the call's own authorization refuses
            // it.
            Err(error) if error.status == StatusCode::FORBIDDEN => {
                return Ok(Admission::default());
            }
            // Anything else fails closed.
            Err(error) => return Err(error),
        };
        let mut admission = Admission::default();
        if principal.role != "owner" && !extensions.is_empty() {
            let mut request = AccessRequest {
                access,
                arguments: arguments.map(Cow::Borrowed),
                max_tokens: None,
            };
            extensions.authorize(db, &principal, &mut request)?;
            if let Some(Cow::Owned(narrowed)) = request.arguments {
                admission.arguments = Some(narrowed);
            }
            admission.cap = request.max_tokens.filter(|_| read);
            if metered && !matches!(call.channel, Channel::Cli | Channel::Hook) {
                let served = Served {
                    repository: &call.repository,
                    session: &call.session,
                    channel: call.channel,
                    kind: &call.kind,
                    unrecorded_tokens: unrecorded(store, &call.workspace, &principal.credential),
                };
                extensions.served_allowed(db, &principal, &served)?;
            }
        }
        if metered {
            call.credential = principal.credential;
            admission.call = Some(call);
        }
        Ok(admission)
    })
}

/// [`admit`] for an HTTP operation, on a blocking thread when there is
/// anything to ask: authorized as `Access::Operation`, metered when [`meter`]
/// meters it, and size-checked when it is a [`Meter::Read`]. Its project is
/// the arguments' `repository_id`.
pub async fn admit_request(
    store: &Store,
    workspace: &str,
    token: &str,
    headers: &HeaderMap,
    operation: &str,
    args: &Value,
) -> Result<Admission> {
    let metered = meter(operation);
    if metered.is_none() && crate::extension::installed().is_empty() {
        return Ok(Admission::default());
    }
    let call = Call {
        workspace: workspace.into(),
        repository: args
            .get("repository_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        credential: String::new(),
        session: String::new(),
        channel: Channel::of(headers),
        kind: operation.into(),
    };
    let (store, token, operation, args) = (
        store.clone(),
        token.to_owned(),
        operation.to_owned(),
        args.clone(),
    );
    crate::product::blocking(move || {
        admit(
            &store,
            &token,
            Ask {
                access: Access::Operation(&operation),
                arguments: Some(&args),
                call,
                metered: metered.is_some(),
                read: metered == Some(Meter::Read),
            },
        )
    })
    .await
}

/// The response to an HTTP call an installed extension refused before it
/// ran: the refusal's status and message and, for a 429 (a limit of the
/// current UTC month), `Retry-After` until the next UTC month starts, as the
/// request budget gives it.
pub fn refusal(error: Error) -> Response {
    let limited = error.status == StatusCode::TOO_MANY_REQUESTS;
    let mut response = error.into_response();
    if limited {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            crate::product::until_next_month(),
        );
    }
    response
}

/// Serve an admitted HTTP result ([`Admission::serve_value`]) off the async
/// executor: the size check of a read and the count of a metered result.
pub async fn served(store: &Store, admission: Admission, result: Value) -> Result<Value> {
    if admission.is_idle() {
        return Ok(result);
    }
    let store = store.clone();
    crate::product::blocking(move || {
        admission.serve_value(&store, &result)?;
        Ok(result)
    })
    .await
}

/// Marks a response whose result the operation's admission has size-checked
/// already, so the route boundary does not count it again.
#[derive(Clone, Copy, Debug)]
pub struct SizeChecked;

/// Serve `operation` from a route of its own (outside the command routes) as
/// the command routes serve it: admitted first ([`admit_request`],
/// [`refusal`]), then `run`, then its result served ([`served`]).
/// `repository` is empty for a route that names no project.
pub async fn serve_route(
    store: &Store,
    headers: &HeaderMap,
    workspace: &str,
    repository: &str,
    operation: &str,
    run: impl Future<Output = Result<Value>>,
) -> Response {
    let token = match crate::api::token(headers) {
        Ok(token) => token,
        Err(error) => return error.into_response(),
    };
    let args = json!({ "repository_id": repository });
    let admission = match admit_request(store, workspace, token, headers, operation, &args).await {
        Ok(admission) => admission,
        Err(error) => return refusal(error),
    };
    let result = match run.await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    match served(store, admission, result).await {
        Ok(value) => {
            let mut response = Json(value).into_response();
            response.extensions_mut().insert(SizeChecked);
            response
        }
        Err(error) => error.into_response(),
    }
}

/// The local command line printed `text`, the result of `operation` from
/// `repository`, to `token`'s credential: count it when the operation is
/// metered ([`meter`]). Never fails the command.
pub fn served_locally(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
    operation: &str,
    text: &str,
) {
    if meter(operation).is_some()
        && !repository.is_empty()
        && let Some(principal) = principal(store, workspace, token)
    {
        let call = Call {
            workspace: workspace.into(),
            repository: repository.into(),
            credential: principal.credential,
            session: String::new(),
            channel: Channel::Cli,
            kind: operation.into(),
        };
        record_text(store, call, text.to_string());
    }
}

/// What [`summary`] and the reports say about the counts.
pub const SCOPE: &str = "Tokens of what Graphicle returned: every MCP tool result and MCP resource read; over HTTP and through graphicle call and graphicle context, every operation an MCP tool runs and the other reads of a project's sources, memory, evidence or graph; and capture hooks' session-start context; counted with cl100k_base on the text served. A result that names no project but serves several (such as a recall over every project of the workspace) is counted on those projects, split by the bytes served from each. The other operations and routes, which return settings, counts or listings and no project text (for example observer.status, list_projects and the project overview), are not counted. This is what Graphicle served, not your agents' bill: Graphicle does not see or count their own model calls, their prompts, other tools and output are not counted here, and their models may count tokens differently.";

/// The served counters of `workspace` from day `from` through day `to`
/// (UTC, `YYYY-MM-DD`): totals, and by channel, tool or operation,
/// credential, agent session, project and day. Sessions and the other lists
/// are cut to their largest entries.
pub fn summary(db: &Connection, workspace: &str, from: &str, to: &str) -> Result<Value> {
    let scope = params![workspace, from, to];
    let totals = rows(
        db,
        "SELECT coalesce(sum(calls),0) AS calls,coalesce(sum(tokens),0) AS tokens FROM am_served_usage WHERE workspace_id=? AND day>=? AND day<=?",
        scope,
    )?
    .pop()
    .unwrap_or_else(|| json!({"calls":0,"tokens":0}));
    let by_channel = rows(
        db,
        "SELECT channel,sum(calls) AS calls,sum(tokens) AS tokens FROM am_served_usage WHERE workspace_id=? AND day>=? AND day<=? GROUP BY channel ORDER BY tokens DESC,channel",
        scope,
    )?;
    let by_kind = rows(
        db,
        "SELECT channel,kind,sum(calls) AS calls,sum(tokens) AS tokens FROM am_served_usage WHERE workspace_id=? AND day>=? AND day<=? GROUP BY channel,kind ORDER BY tokens DESC,channel,kind LIMIT 100",
        scope,
    )?;
    let by_credential = rows(
        db,
        "SELECT u.credential_id,CASE WHEN u.credential_id='owner' THEN 'Workspace owner' ELSE c.label END AS label,
         CASE WHEN u.credential_id='owner' THEN 'owner' ELSE c.role END AS role,c.revoked_at,
         sum(u.calls) AS calls,sum(u.tokens) AS tokens
         FROM am_served_usage u LEFT JOIN credentials c ON c.id=u.credential_id AND c.workspace_id=u.workspace_id
         WHERE u.workspace_id=? AND u.day>=? AND u.day<=? GROUP BY u.credential_id ORDER BY tokens DESC,u.credential_id LIMIT 200",
        scope,
    )?;
    let by_session = rows(
        db,
        "SELECT session,credential_id,group_concat(DISTINCT channel) AS channels,min(day) AS first_day,max(day) AS last_day,
         sum(calls) AS calls,sum(tokens) AS tokens FROM am_served_usage
         WHERE workspace_id=? AND day>=? AND day<=? AND session<>'' GROUP BY session,credential_id ORDER BY tokens DESC,session LIMIT 50",
        scope,
    )?;
    let by_project = rows(
        db,
        "SELECT u.repository_id,r.name,sum(u.calls) AS calls,sum(u.tokens) AS tokens FROM am_served_usage u
         JOIN repo_projects r ON r.id=u.repository_id WHERE u.workspace_id=? AND u.day>=? AND u.day<=?
         GROUP BY u.repository_id ORDER BY tokens DESC,u.repository_id LIMIT 100",
        scope,
    )?;
    let daily = rows(
        db,
        "SELECT day,sum(calls) AS calls,sum(tokens) AS tokens FROM am_served_usage WHERE workspace_id=? AND day>=? AND day<=? GROUP BY day ORDER BY day",
        scope,
    )?;
    Ok(json!({
        "calls":totals["calls"],"tokens":totals["tokens"],"counted_with":"cl100k_base",
        "by_channel":by_channel,"by_kind":by_kind,"by_credential":by_credential,
        "by_session":by_session,"by_project":by_project,"daily":daily,"scope":SCOPE
    }))
}

/// The model calls Graphicle made for `workspace` from day `from` through
/// day `to` (UTC), uncached and recorded: by credential (the job's
/// requester; `owner` for the owner capability, null for calls outside a
/// job) and by provider protocol and model. Tokens only, no cost.
pub fn model_usage(db: &Connection, workspace: &str, from: &str, to: &str) -> Result<Value> {
    let end = next_day(to)?;
    let by_credential = rows(
        db,
        "SELECT CASE WHEN j.principal_hash IS NULL THEN NULL WHEN j.principal_hash=w.capability_hash THEN 'owner' ELSE cr.id END AS credential_id,
         count(*) AS calls,coalesce(sum(c.input_tokens),0) AS input_tokens,coalesce(sum(c.output_tokens),0) AS output_tokens
         FROM am_model_calls c JOIN repo_projects r ON r.id=c.repository_id JOIN workspaces w ON w.id=r.workspace_id
         LEFT JOIN am_jobs j ON j.id=c.job_id
         LEFT JOIN credentials cr ON cr.capability_hash=j.principal_hash AND cr.workspace_id=r.workspace_id
         WHERE r.workspace_id=? AND c.cached=0 AND c.created_at>=? AND c.created_at<?
         GROUP BY 1 ORDER BY input_tokens+output_tokens DESC LIMIT 200",
        params![workspace, from, end],
    )?;
    let by_model = rows(
        db,
        "SELECT c.provider,c.model,count(*) AS calls,coalesce(sum(c.input_tokens),0) AS input_tokens,coalesce(sum(c.output_tokens),0) AS output_tokens
         FROM am_model_calls c JOIN repo_projects r ON r.id=c.repository_id
         WHERE r.workspace_id=? AND c.cached=0 AND c.created_at>=? AND c.created_at<?
         GROUP BY c.provider,c.model ORDER BY input_tokens+output_tokens DESC,c.provider,c.model LIMIT 200",
        params![workspace, from, end],
    )?;
    Ok(json!({"by_credential":by_credential,"by_model":by_model}))
}

/// The UTC day after `day` (`YYYY-MM-DD`).
pub fn next_day(day: &str) -> Result<String> {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.succ_opt())
        .map(|date| date.format("%Y-%m-%d").to_string())
        .ok_or_else(|| crate::Error::bad("Expected a UTC day as YYYY-MM-DD"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Store, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Metering").unwrap();
        let workspace = created["id"].as_str().unwrap().to_string();
        let owner = created["capability"].as_str().unwrap().to_string();
        let project = store
            .register(
                &workspace,
                &owner,
                &json!({"project_key":"metering","name":"Metering"}),
            )
            .unwrap();
        let repository = project["id"].as_str().unwrap().to_string();
        (dir, store, workspace, owner, repository)
    }

    fn call(workspace: &str, repository: &str, credential: &str, session: &str) -> Call {
        Call {
            workspace: workspace.into(),
            repository: repository.into(),
            credential: credential.into(),
            session: session.into(),
            channel: Channel::Mcp,
            kind: "recall_project".into(),
        }
    }

    #[test]
    fn counts_gather_in_memory_until_a_flush_writes_them_once() {
        let (_dir, store, workspace, _, repository) = fixture();
        let first = call(&workspace, &repository, "owner", "claude-code:abc");
        record(&store, &first, 120);
        record(&store, &first, 30);
        record(&store, &call(&workspace, &repository, "key-1", ""), 7);
        assert_eq!(unrecorded(&store, &workspace, "owner"), 150);
        assert_eq!(unrecorded(&store, &workspace, "key-1"), 7);
        assert_eq!(unrecorded(&store, "elsewhere", "owner"), 0);
        let stored = |store: &Store| -> Vec<Value> {
            store
                .read(|db| {
                    rows(
                        db,
                        "SELECT credential_id,session,channel,kind,calls,tokens FROM am_served_usage ORDER BY credential_id,session",
                        [],
                    )
                })
                .unwrap()
        };
        assert!(stored(&store).is_empty(), "nothing is written on record");
        assert_eq!(flush(&store).unwrap(), 2);
        assert_eq!(unrecorded(&store, &workspace, "owner"), 0);
        let written = stored(&store);
        assert_eq!(written.len(), 2);
        assert_eq!(written[0]["credential_id"], "key-1");
        assert_eq!(written[0]["tokens"], 7);
        assert_eq!(written[1]["session"], "claude-code:abc");
        assert_eq!(written[1]["calls"], 2);
        assert_eq!(written[1]["tokens"], 150);
        // A second batch adds to the same counters.
        record(&store, &first, 50);
        flush(&store).unwrap();
        assert_eq!(stored(&store)[1]["tokens"], 200);
        assert_eq!(flush(&store).unwrap(), 0, "nothing left");
    }

    #[test]
    fn served_text_is_counted_off_the_callers_path_and_a_flush_waits_for_it() {
        let (_dir, store, workspace, _, repository) = fixture();
        let text = "evidence ".repeat(4000);
        let expected = tokens(&text);
        for _ in 0..3 {
            record_text(
                &store,
                call(&workspace, &repository, "key-2", ""),
                text.clone(),
            );
        }
        flush(&store).unwrap();
        let (calls, total): (i64, i64) = store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT sum(calls),sum(tokens) FROM am_served_usage WHERE credential_id='key-2'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!((calls, total as u64), (3, 3 * expected));
    }

    #[test]
    fn a_failed_write_keeps_its_counts_and_a_deleted_project_drops_them() {
        let (_dir, store, workspace, owner, repository) = fixture();
        let blocker = store.connect().unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        record(&store, &call(&workspace, &repository, "owner", ""), 10);
        let short = Waits {
            counting: WAIT,
            batches: Duration::from_millis(50),
            lock: Some(Duration::from_millis(50)),
        };
        assert!(flush_waiting(&store, short).is_err());
        assert_eq!(unrecorded(&store, &workspace, "owner"), 10, "kept");
        blocker.execute_batch("ROLLBACK").unwrap();
        // The project goes before its counts are written: they are dropped,
        // and the rest of the batch is written.
        let other = store
            .register(
                &workspace,
                &owner,
                &json!({"project_key":"other","name":"Other"}),
            )
            .unwrap();
        let other = other["id"].as_str().unwrap();
        record(&store, &call(&workspace, other, "owner", ""), 5);
        store
            .connect()
            .unwrap()
            .execute("DELETE FROM repo_projects WHERE id=?", [other])
            .unwrap();
        assert_eq!(flush(&store).unwrap(), 2);
        let total: i64 = store
            .read(|db| {
                Ok(db.query_row("SELECT sum(tokens) FROM am_served_usage", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(total, 10);
    }

    #[test]
    fn old_session_rows_fold_into_rows_without_a_session_and_old_rows_go() {
        let (_dir, store, workspace, _, repository) = fixture();
        let db = store.connect().unwrap();
        let day = |days: i64| {
            (chrono::Utc::now() - chrono::Duration::days(days))
                .format("%Y-%m-%d")
                .to_string()
        };
        for (session, days, tokens) in [
            ("a:1", SESSION_DAYS + 5, 10),
            ("b:2", SESSION_DAYS + 5, 20),
            ("", SESSION_DAYS + 5, 1),
            ("c:3", 1, 40),
            ("d:4", KEEP_DAYS + 3, 50),
        ] {
            db.execute(
                "INSERT INTO am_served_usage VALUES(?,?,?,'owner',?,'mcp','recall_project',1,?)",
                params![repository, workspace, day(days), session, tokens],
            )
            .unwrap();
        }
        record(&store, &call(&workspace, &repository, "owner", ""), 0);
        flush(&store).unwrap();
        let left = rows(
            &db,
            "SELECT day,session,calls,tokens FROM am_served_usage WHERE tokens>0 ORDER BY day",
            [],
        )
        .unwrap();
        assert_eq!(
            left,
            vec![
                json!({"day":day(SESSION_DAYS + 5),"session":"","calls":3,"tokens":31}),
                json!({"day":day(1),"session":"c:3","calls":1,"tokens":40}),
            ]
        );
    }

    #[test]
    fn session_labels_never_keep_a_clients_own_id() {
        let (_dir, store, workspace, _, _) = fixture();
        let label = session_label("Claude Code", "8c1f-secret-session");
        assert!(label.starts_with("claudecode:"), "{label}");
        assert_eq!(label.len(), "claudecode:".len() + 12);
        assert!(!label.contains("secret"));
        assert_eq!(client_name("  "), "agent");
        assert_eq!(client_name("Cursor/1.2 (beta)"), "cursor1.2beta");
        let issued = issue_http_session(&store, &workspace, "Cursor/1.2").unwrap();
        let (signed, signature) = issued.rsplit_once('.').unwrap();
        let random = signed.strip_prefix("cursor1.2.").unwrap();
        assert_eq!((random.len(), signature.len()), (32, 64), "{issued}");
        assert!(issued.len() <= 200, "the route reads ids up to 200 bytes");
        assert_eq!(
            http_session(&store, &workspace, &issued),
            format!("cursor1.2:{}", &random[..12])
        );
        assert_ne!(
            issue_http_session(&store, &workspace, "Cursor/1.2"),
            Some(issued.clone())
        );
        assert!(new_session("codex").starts_with("codex:"));
        assert_ne!(new_session("codex"), new_session("codex"));
    }

    /// A client could send a new `Mcp-Session-Id` with every call and so add
    /// a counter row each time; only ids this server signed for the
    /// workspace keep a label of their own.
    #[test]
    fn session_ids_this_server_did_not_sign_share_one_label() {
        let (_dir, store, workspace, _, _) = fixture();
        let issued = issue_http_session(&store, &workspace, "claude-code").unwrap();
        let other = store.create_workspace("Other").unwrap();
        let elsewhere = other["id"].as_str().unwrap();
        let (signed, signature) = issued.rsplit_once('.').unwrap();
        let mut flipped = signature.to_string();
        flipped.replace_range(..1, if signature.starts_with('0') { "1" } else { "0" });
        let random = &signed["claude-code.".len()..];
        for forged in [
            "Client-Chosen-Session".to_string(),
            format!("claude-code.{random}"),
            format!("{signed}.{flipped}"),
            format!("codex.{random}.{signature}"),
            format!("{signed}.{}", signature.to_uppercase()),
            format!("{signed}.{signature}0"),
            format!("{signed}.{}", "é".repeat(32)),
            String::new(),
        ] {
            assert_eq!(
                http_session(&store, &workspace, &forged),
                UNVERIFIED_SESSION,
                "{forged}"
            );
        }
        // Another workspace's key does not sign this one's ids.
        assert_eq!(http_session(&store, elsewhere, &issued), UNVERIFIED_SESSION);
        assert_eq!(issue_http_session(&store, "no-such-workspace", "x"), None);
    }

    #[test]
    fn every_operation_a_tool_runs_and_the_other_text_reads_are_metered() {
        for (operation, expected) in [
            ("context", Some(Meter::Read)),
            ("read", Some(Meter::Read)),
            ("evidence.read", Some(Meter::Read)),
            ("status", Some(Meter::Read)),
            ("runtime.status", Some(Meter::Read)),
            ("memories", Some(Meter::Read)),
            ("memory.history", Some(Meter::Read)),
            ("interpretation.prepare", Some(Meter::Read)),
            ("learning.refresh.prepare", Some(Meter::Read)),
            ("session.timeline", Some(Meter::Read)),
            ("work_state.read", Some(Meter::Read)),
            ("workflow.status", Some(Meter::Read)),
            ("memory.export", Some(Meter::Read)),
            ("document.original", Some(Meter::Read)),
            ("claim.history", Some(Meter::Read)),
            ("graph.source", Some(Meter::Read)),
            ("graph.neo4j.search", Some(Meter::Read)),
            ("graph.doc_links", Some(Meter::Read)),
            ("graph.navigate", Some(Meter::Read)),
            ("project.list", Some(Meter::Read)),
            ("graph.link", Some(Meter::Write)),
            ("memory.propose", Some(Meter::Write)),
            ("interpretation.propose", Some(Meter::Write)),
            ("learning.refresh.propose", Some(Meter::Write)),
            ("work_state.write", Some(Meter::Write)),
            ("observer.status", None),
            ("connector.list", None),
            ("model.status", None),
            ("learning.review", None),
            ("memory.import", None),
            ("build.info", None),
            ("retention.preview", None),
            ("graph.project", None),
        ] {
            assert_eq!(meter(operation), expected, "{operation}");
        }
        assert_eq!(tools_for("read"), ["read_project_source"]);
        assert_eq!(tools_for("evidence.read"), ["read_project_source"]);
        assert_eq!(tools_for("memories"), ["project_memory"]);
        // The graph engine's navigation tools run one operation.
        let navigation = tools_for("graph.navigate");
        assert!(
            ["query_graph", "get_node", "get_neighbors"]
                .iter()
                .all(|tool| navigation.iter().any(|listed| listed == tool)),
            "{navigation:?}"
        );
        assert!(tools_for("memory.export").is_empty());
        assert!(tools_for("observer.status").is_empty());
        // The other reads are Core operations no published tool runs.
        for operation in TEXT_READS {
            assert!(
                crate::api::CORE_OPERATIONS.contains(operation),
                "{operation}"
            );
            assert!(tools_for(operation).is_empty(), "{operation}");
        }
        for (alias, operation) in ALIASES {
            assert!(crate::api::CORE_OPERATIONS.contains(alias));
            assert_eq!(meter(alias), meter(operation));
        }
        // A write a read-only tool runs: its result is never refused.
        for operation in WRITES {
            assert!(!tools_for(operation).is_empty(), "{operation}");
            assert_eq!(meter(operation), Some(Meter::Write));
        }
    }

    /// A usage report records this process's counts first, but waits neither
    /// for results still being counted nor long for a busy store: what is
    /// left is reported as unrecorded.
    #[test]
    fn a_reports_flush_is_brief_and_leaves_what_it_cannot_write_unrecorded() {
        let (_dir, store, workspace, _, repository) = fixture();
        record(&store, &call(&workspace, &repository, "key-1", ""), 12);
        // A result still being counted on its thread is not waited for.
        Pending::of(&mut registry(), &store).counting += 1;
        let started = Instant::now();
        assert_eq!(flush_briefly(&store).unwrap(), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        counted(&store);
        assert_eq!(unrecorded_in(&store, &workspace), 0);
        // A busy store: the report goes on at once, and the counts stay.
        let blocker = store.connect().unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        record(&store, &call(&workspace, &repository, "key-1", ""), 30);
        let started = Instant::now();
        assert!(flush_briefly(&store).is_err());
        assert!(started.elapsed() < REPORT_WAIT + Duration::from_secs(2));
        assert_eq!(unrecorded_in(&store, &workspace), 30);
        let today = today();
        assert_eq!(unrecorded_between(&store, &workspace, &today, &today), 30);
        assert_eq!(
            unrecorded_between(&store, &workspace, "2020-01-01", "2020-01-31"),
            0
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(flush(&store).unwrap(), 1);
    }

    /// Only the owner's usage report records counts, and only once the owner
    /// is authorized; its `unrecorded_tokens` names what is left.
    #[test]
    fn the_usage_report_authorizes_the_owner_before_it_writes() {
        let (_dir, store, workspace, owner, repository) = fixture();
        let key = store
            .grant(&workspace, &owner, &json!({"role":"agent","label":"Agent"}))
            .unwrap();
        let key = key["capability"].as_str().unwrap();
        record(&store, &call(&workspace, &repository, "key-1", ""), 40);
        let refused = store.product_usage(&workspace, key).unwrap_err();
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(unrecorded_in(&store, &workspace), 40, "nothing written");
        let usage = store.product_usage(&workspace, &owner).unwrap();
        assert_eq!(usage["served"]["tokens"], 40);
        assert_eq!(usage["served"]["unrecorded_tokens"], 0);
        // A busy store: the report still answers, with the counts left over.
        let blocker = store.connect().unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        record(&store, &call(&workspace, &repository, "key-1", ""), 2);
        let usage = store.product_usage(&workspace, &owner).unwrap();
        assert_eq!(usage["served"]["tokens"], 40);
        assert_eq!(usage["served"]["unrecorded_tokens"], 2);
        blocker.execute_batch("ROLLBACK").unwrap();
        flush(&store).unwrap();
    }

    #[test]
    fn a_quota_refusal_gives_retry_after_until_the_next_utc_month() {
        let limited = refusal(Error {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "quota".into(),
            provider_failure: None,
        });
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let seconds: i64 = limited.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=31 * 86_400).contains(&seconds), "{seconds}");
        let denied = refusal(Error::denied());
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(denied.headers().get("retry-after").is_none());
    }

    /// A call that names no project (a workspace recall) is counted on the
    /// projects its result serves: tokens split by bytes, the call once on
    /// the largest share; a result that names none is not counted.
    #[test]
    fn a_call_that_names_no_project_is_counted_on_the_projects_it_serves() {
        let (_dir, store, workspace, owner, a) = fixture();
        let b = store
            .register(&workspace, &owner, &json!({"project_key":"b","name":"B"}))
            .unwrap();
        let b = b["id"].as_str().unwrap().to_string();
        let packet = json!({"sources":[
            {"repository_id":a,"text":"x".repeat(300)},
            {"repository_id":b,"text":"y".repeat(100)},
            {"id":"no project"}
        ]});
        let parts = shares(&packet);
        assert_eq!(parts.len(), 2);
        let text = "evidence ".repeat(500);
        let expected = tokens(&text);
        record_served(&store, call(&workspace, "", "key-1", ""), text, parts);
        record_served(
            &store,
            call(&workspace, "", "key-1", ""),
            "listing".into(),
            shares(&json!({"projects":[{"id":a}]})),
        );
        flush(&store).unwrap();
        let counted = store
            .read(|db| {
                rows(
                    db,
                    "SELECT repository_id,calls,tokens FROM am_served_usage ORDER BY tokens DESC",
                    [],
                )
            })
            .unwrap();
        assert_eq!(counted.len(), 2, "{counted:?}");
        assert_eq!(counted[0]["repository_id"], a.as_str());
        assert_eq!(counted[0]["calls"], 1);
        assert_eq!(counted[1]["calls"], 0);
        let total: u64 = counted
            .iter()
            .map(|row| row["tokens"].as_u64().unwrap())
            .sum();
        assert_eq!(total, expected);
        assert!(counted[1]["tokens"].as_u64().unwrap() > 0);
        // Digest sections name their projects too.
        let digest = json!({"repositories":[{"repository_id":b,"changes":[1,2,3]}]});
        assert_eq!(shares(&digest)[0].0, b);
    }

    /// One credential keeps at most SESSIONS_PER_DAY session labels a day;
    /// further sessions share one label, and known ones keep theirs.
    #[test]
    fn a_credential_keeps_at_most_its_daily_sessions() {
        let (_dir, store, workspace, _, repository) = fixture();
        for n in 0..SESSIONS_PER_DAY + 3 {
            record(
                &store,
                &call(&workspace, &repository, "key-1", &format!("agent:{n:04}")),
                1,
            );
        }
        record(
            &store,
            &call(&workspace, &repository, "key-2", "agent:0000"),
            1,
        );
        flush(&store).unwrap();
        // A session kept earlier keeps its label in a later batch.
        record(
            &store,
            &call(&workspace, &repository, "key-1", "agent:0000"),
            1,
        );
        record(
            &store,
            &call(&workspace, &repository, "key-1", "agent:9999"),
            1,
        );
        flush(&store).unwrap();
        let sessions = |credential: &str| -> Vec<Value> {
            store
                .read(|db| {
                    rows(
                        db,
                        "SELECT session,calls FROM am_served_usage WHERE credential_id=? ORDER BY session",
                        [credential],
                    )
                })
                .unwrap()
        };
        let kept = sessions("key-1");
        assert_eq!(kept.len() as i64, SESSIONS_PER_DAY + 1);
        let other = kept
            .iter()
            .find(|row| row["session"] == OTHER_SESSIONS)
            .unwrap();
        assert_eq!(other["calls"], 4);
        assert_eq!(kept[0]["session"], "agent:0000");
        assert_eq!(kept[0]["calls"], 2);
        assert_eq!(sessions("key-2").len(), 1, "another credential's own day");
    }

    /// The caller is resolved by authentication only: a credential whose
    /// role grants no `read` is still known, so it is checked and counted.
    #[test]
    fn a_principal_is_resolved_by_authentication_only() {
        let (_dir, store, workspace, owner, _) = fixture();
        let granted = store
            .grant(
                &workspace,
                &owner,
                &json!({"role":"agent","label":"Narrow"}),
            )
            .unwrap();
        let token = granted["capability"].as_str().unwrap();
        store
            .connect()
            .unwrap()
            .execute(
                "UPDATE credentials SET role='writer-only' WHERE id=?",
                [granted["id"].as_str().unwrap()],
            )
            .unwrap();
        assert!(
            store
                .authorized_read(&workspace, token, "read", |_, _| Ok(()))
                .is_err()
        );
        let found = principal(&store, &workspace, token).unwrap();
        assert_eq!(found.credential, granted["id"].as_str().unwrap());
        assert!(principal(&store, &workspace, "not-a-token").is_none());
        assert_eq!(principal(&store, &workspace, &owner).unwrap().role, "owner");
    }

    #[test]
    fn the_web_app_is_labelled_apart_from_api_clients() {
        let mut headers = HeaderMap::new();
        assert_eq!(Channel::of(&headers), Channel::Http);
        headers.insert("x-graphicle-request", "1".parse().unwrap());
        assert_eq!(Channel::of(&headers), Channel::Web);
        assert_eq!(next_day("2026-12-31").unwrap(), "2027-01-01");
        assert!(next_day("tomorrow").is_err());
    }
}
