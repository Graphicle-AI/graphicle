//! Extension registry: how an edition adds features to the runtime.
//!
//! Core is this runtime with no extensions installed ([`Extensions::none`]).
//! Another edition passes its extensions to the same program entry point,
//! [`crate::cli::main_with`], so every edition runs the same CLI, HTTP and MCP
//! code and differs only in what is installed. Every hook has a default, so an
//! extension implements only what it adds, and Core needs no code at all.
//!
//! The registry is process-global ([`install`] once, [`installed`] anywhere)
//! because several of the places that consult it are free functions with no
//! [`App`]: `api::mcp_tools`, [`crate::native`], `Store::open` and
//! `store::authorize_hash`.
//!
//! Where the runtime calls each hook:
//! - `edition`, `capabilities`: the `build.info` operation, `--version` and
//!   the update check (`core_capabilities`);
//! - `migrate`: `Store::open`, after the core schema;
//! - `operations`, `command`, `command_async`: `api::command` and
//!   `api::command_async`, before the core operations;
//! - `routes`: `api::router`, inside the request meter and the host, origin
//!   and browser-session boundary;
//! - `mcp_tools`: `api::mcp_tools_with` and MCP `tools/call`;
//! - `native_operations`, `native`: [`crate::native`], for the declared
//!   operations only, before the memory engine;
//! - `session_credential`, `credential_allowed`: `store::authorize_hash`;
//! - `credential_revoked`: `Store::revoke`;
//! - `request_allowance`, `usage_details`: the request meter and the owner's
//!   usage report (`product`);
//! - `scrub_backup`: `Store::backup`, on the copy;
//! - `job_kinds`, `job`: queueing, leasing and `workflows::execute`, for the
//!   declared job kinds (the core runs its own); `job` is asked in
//!   installation order, not routed by declaration (see [`Extensions::job`]);
//! - `after_projection`: `jobs::run_projection`, in the transaction that
//!   recorded an episode; the projection drain (`jobs::drain_projection`)
//!   runs the jobs it queues;
//! - `schedule`: `Store::schedule_connectors`, the worker's scheduler;
//! - `source_permissions`: provider permissions captured during a connector
//!   sync;
//! - `environment`, `serve`: `graphicle serve` and `graphicle start`, before
//!   the server announces readiness; its background tasks run until shutdown;
//! - `cli`, `run_cli`: the `graphicle` command line ([`crate::cli`]), for the
//!   subcommands the extension adds;
//! - `ui_script`: the `/extensions.js` script of the web app;
//! - `page`: the public `/product` page (`product`);
//! - `history_tables`, `retained_tables`: verified deletion (`deletion`), for
//!   `deletion.state` and the residue scans of deletion receipts.
use crate::{
    Error, Result,
    api::App,
    connectors::Remote,
    jobs::{Job, Worker},
    store::{Principal, Store},
};
use axum::Router;
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::OnceLock,
};

/// A boxed, sendable future, so hooks can be async without a new dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Core operations an extension may take over. With no extension serving
/// them, the core answers them with 501 after authorization
/// (`core_capabilities::not_included`); their HTTP routes run through the same
/// operation table.
pub const OVERRIDABLE_OPERATIONS: &[&str] = &["impact.report", "coverage.upload", "graph.impact"];

/// The edition a program is. An extension declares the edition it makes
/// ([`Extension::edition`]); installing an extension does not by itself make
/// a program another edition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    Core,
    Pro,
}

impl Edition {
    /// The name `build.info`, `graphicle --version` and release feeds use.
    pub fn name(self) -> &'static str {
        match self {
            Edition::Core => "core",
            Edition::Pro => "pro",
        }
    }
}

/// A job kind an extension runs through [`Extension::job`].
#[derive(Clone, Copy, Debug)]
pub struct JobKind {
    /// The `kind` of its jobs; never a kind the core knows.
    pub name: &'static str,
    /// The permission its requester needs (`read`, `propose`, `run` or
    /// `admin`), checked when it is queued and at every lease.
    pub permission: &'static str,
    /// Source work, like a connector sync: no model call and no pinned
    /// source state.
    pub source: bool,
    /// Work that follows a projection: the
    /// extension queues it from [`Extension::after_projection`], and the
    /// projection drain (`jobs::drain_projection`) runs it after the
    /// `project` and `link` jobs, so it runs wherever projections are drained
    /// (`sync`, `mcp`, `ask`, `run --wait`, `pr`), not only in a worker. The
    /// drain runs it with a worker that has no model configuration, so such a
    /// kind must make no model call.
    pub after_projection: bool,
}

impl JobKind {
    pub const fn new(name: &'static str, permission: &'static str, source: bool) -> Self {
        Self {
            name,
            permission,
            source,
            after_projection: false,
        }
    }
    /// This kind, as work that follows projections
    /// ([`Self::after_projection`]).
    pub const fn following_projection(self) -> Self {
        Self {
            after_projection: true,
            ..self
        }
    }
}

/// An MCP tool contributed by an extension.
pub struct McpTool {
    /// The published tool object: `name`, `description`, `inputSchema` and
    /// `annotations` (see `api::tool`).
    pub spec: Value,
    /// The operation `tools/call` runs, through `api::command`.
    pub operation: &'static str,
    /// Whether a non-empty `repo` argument selects another repository of the
    /// workspace (re-authorized under the caller) instead of the connection's.
    pub repo_arg: bool,
    /// Whether the tool reads or acts on the workspace as a whole, needing no
    /// project. A workspace connection (`/mcp/{workspace}`,
    /// `graphicle mcp --workspace`) publishes these tools and the tools with
    /// a `repo` argument, which it then requires.
    pub workspace: bool,
    /// Whether the tool reaches beyond the connection's project on workspace
    /// connections only: a project
    /// connection publishes it without its `repo` argument, or not at all
    /// when it has none (`list_projects`), and so reads its own project only,
    /// as before workspace connections. Opting in to the workspace is
    /// choosing a workspace
    /// connection. The graph tools' `repo` is not affected.
    pub workspace_only: bool,
}

impl McpTool {
    pub fn new(spec: Value, operation: &'static str, repo_arg: bool) -> Self {
        Self {
            spec,
            operation,
            repo_arg,
            workspace: false,
            workspace_only: false,
        }
    }
    /// The tool needs no project (see [`McpTool::workspace`]).
    pub fn workspace_level(mut self) -> Self {
        self.workspace = true;
        self
    }
    /// The tool reaches other projects on workspace connections only (see
    /// the `workspace_only` field).
    pub fn workspace_only(mut self) -> Self {
        self.workspace_only = true;
        self
    }
    /// The tool as a project connection publishes it: unchanged, without its
    /// `repo` argument, or not at all (see the `workspace_only` field).
    pub fn confined(mut self) -> Option<Self> {
        if !self.workspace_only {
            return Some(self);
        }
        if !self.repo_arg {
            return None;
        }
        if let Some(properties) = self.spec["inputSchema"]["properties"].as_object_mut() {
            properties.shift_remove("repo");
        }
        self.repo_arg = false;
        Some(self)
    }
    /// The tool's published name.
    pub fn name(&self) -> &str {
        self.spec["name"].as_str().unwrap_or("")
    }
}

/// What `graphicle serve` gives the `serve` hook, before it announces that
/// the server is ready.
pub struct ServeContext<'a> {
    pub store: &'a Store,
    /// The HTTPS origin of a reverse proxy (`--public-origin`), if any.
    pub public_origin: Option<&'a str>,
    /// Tasks to run beside the server; they are stopped at shutdown.
    pub background: Vec<BoxFuture<'static, ()>>,
}

/// What an extension command gets from the `graphicle` command line: the
/// global options and the local store, opened only when a command asks for it,
/// so commands that need no store never create the data folder.
pub struct CliContext {
    /// `--data-dir`, else the default local data folder.
    pub data_dir: PathBuf,
    /// `--config`, if given.
    pub config: Option<PathBuf>,
    store: OnceLock<Store>,
}

impl CliContext {
    pub fn new(data_dir: PathBuf, config: Option<PathBuf>) -> Self {
        Self {
            data_dir,
            config,
            store: OnceLock::new(),
        }
    }
    /// The local store, opened (and created if needed) on first use.
    pub fn store(&self) -> Result<&Store> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let store = Store::open(&self.data_dir)?;
        Ok(self.store.get_or_init(|| store))
    }
}

/// Features an edition adds to the runtime. Every method has a default that
/// adds nothing, so an implementation overrides only the hooks it uses.
pub trait Extension: Send + Sync + 'static {
    /// A short, stable name used in diagnostics.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
    /// The edition this extension makes the program; `None` leaves it Core.
    /// `build.info`, `graphicle --version` and the update check report the
    /// declared edition (all declaring extensions must agree, see
    /// [`install`]).
    fn edition(&self) -> Option<Edition> {
        None
    }
    /// Adjust the `build.info` capabilities, for example set a feature flag.
    /// The `edition` field is not an extension's to change ([`Self::edition`]).
    fn capabilities(&self, _capabilities: &mut Map<String, Value>) {}
    /// Create or upgrade this extension's tables. It runs on every store open,
    /// after the core schema, so it may reference core tables. Statements must
    /// be idempotent (`CREATE ... IF NOT EXISTS`); stores keep every table, so
    /// an edition without this extension simply ignores them.
    fn migrate(&self, _db: &Connection) -> Result<()> {
        Ok(())
    }
    /// The operations this extension serves through [`Self::command`] and
    /// [`Self::command_async`]. They must not repeat a core operation, except
    /// the [`OVERRIDABLE_OPERATIONS`], nor another extension's ([`install`]
    /// panics).
    fn operations(&self) -> &'static [&'static str] {
        &[]
    }
    /// Run one of [`Self::operations`] synchronously (HTTP, MCP and
    /// `graphicle call`). Arguments are always a JSON object. The default
    /// rejects the operation the way the core rejects an unknown one, which is
    /// right for operations that exist only in [`Self::command_async`].
    fn command(
        &self,
        _app: &App,
        _workspace: &str,
        _capability: &str,
        _operation: &str,
        _args: &Value,
    ) -> Result<Value> {
        Err(Error::bad("Unsupported native project operation"))
    }
    /// Run one of [`Self::operations`] on the async HTTP path, for operations
    /// that await network I/O. `None` falls back to [`Self::command`] on a
    /// blocking thread.
    fn command_async<'a>(
        &'a self,
        _app: &'a App,
        _workspace: &'a str,
        _capability: &'a str,
        _operation: &'a str,
        _args: &'a Value,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        None
    }
    /// HTTP routes. They are merged inside the core layers, so they keep the
    /// host, origin and browser-session checks and the request meter. Routes
    /// must not overlap core routes (the router panics). State an extension
    /// owns is reached through `axum::Extension` layered on its own router.
    fn routes(&self) -> Router<App> {
        Router::new()
    }
    /// MCP tools, published after the core tools. Names must be unique.
    fn mcp_tools(&self, _learning: bool, _work_state: bool) -> Vec<McpTool> {
        Vec::new()
    }
    /// The native (engine) operations [`Self::native`] serves, by full name in
    /// the engine's `graph.` namespace, for example `graph.impact.diff`. Only
    /// these reach it. [`install`] rejects a name outside that namespace, one
    /// the memory engine's own graph layer serves, or one another extension
    /// declares; the core's own native calls never use the namespace.
    fn native_operations(&self) -> &'static [&'static str] {
        &[]
    }
    /// Serve one of [`Self::native_operations`] before the memory engine
    /// would. `operation` is the full name and the request is a JSON object
    /// without `operation`; the extension may take it (`std::mem::take`).
    /// `None` leaves the request to the memory engine.
    fn native(&self, _operation: &str, _request: &mut Value) -> Option<Result<Value>> {
        None
    }
    /// The credential row (`id`, `role`, `expires_at`, `revoked_at`,
    /// `capability_hash`) for a token hash that matches no workspace
    /// credential, such as a browser session; `None` when it is not one.
    fn session_credential(
        &self,
        _db: &Connection,
        _workspace: &str,
        _hash: &str,
    ) -> Result<Option<Value>> {
        Ok(None)
    }
    /// Whether a workspace credential may still be used. It is consulted on
    /// every authorization of a credential other than the owner capability.
    fn credential_allowed(&self, _db: &Connection, _credential: &str) -> Result<bool> {
        Ok(true)
    }
    /// A credential was revoked, inside the revoking transaction.
    fn credential_revoked(
        &self,
        _db: &Connection,
        _workspace: &str,
        _credential: &str,
    ) -> Result<()> {
        Ok(())
    }
    /// The workspace's monthly HTTP request allowance from this extension (a
    /// plan, for example); `None` sets no limit. The lowest of the owner's own
    /// budget and every allowance applies.
    fn request_allowance(&self, _db: &Connection, _workspace: &str) -> Result<Option<u64>> {
        Ok(None)
    }
    /// Add fields to the owner's usage report.
    fn usage_details(&self, _db: &Connection, _workspace: &str, _usage: &mut Value) -> Result<()> {
        Ok(())
    }
    /// Scrub a portable backup copy (never the live store) after the core
    /// scrub: remove sessions, secrets or entitlements the copy must not carry.
    fn scrub_backup(&self, _db: &Connection) -> Result<()> {
        Ok(())
    }
    /// The job kinds this extension runs through [`Self::job`], with the
    /// permission each needs. Declared kinds can be queued and are leased; a
    /// job of a kind no installed extension declares is never leased, so a
    /// program that runs it still finds it queued, and job status reports it
    /// as waiting. [`install`] rejects a kind the core knows, an unsupported
    /// kind or one another extension declares.
    fn job_kinds(&self) -> &'static [JobKind] {
        &[]
    }
    /// Run a leased job of a kind the core does not run (one of
    /// [`Self::job_kinds`], or the core's `permissions` kind when this
    /// extension syncs provider permissions); `None` for every other kind.
    /// Jobs are not routed by declaration ([`Extensions::job`]), so accepting
    /// a kind another extension runs would take that extension's jobs.
    fn job<'a>(
        &'a self,
        _worker: &'a Worker,
        _job: &'a Job,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        None
    }
    /// Queue work that follows a projection. It is
    /// called in the transaction of a `project` job that recorded (or
    /// replayed) `episode` of `repository`, after the core queued its `link`
    /// job, with that job's principal. Return the jobs it queued, as
    /// `{id, kind, state, replayed}`; their kinds must be declared
    /// [`JobKind::after_projection`], so the projection drain runs them. An
    /// error is reported in the projection's result and rolls back only this
    /// hook's writes; it never fails the projection. The default queues
    /// nothing.
    fn after_projection(
        &self,
        _db: &Connection,
        _principal: &Principal,
        _repository: &str,
        _episode: &str,
    ) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    /// Queue this extension's scheduled jobs; returns how many were queued.
    /// The worker calls it about once a minute.
    fn schedule(&self, _store: &Store) -> Result<usize> {
        Ok(0)
    }
    /// Fetch the provider-side permissions of a document during a connector
    /// sync in `permission_mode: provider`, as `{complete, principals,
    /// checked_at, ...}`; `None` when this extension does not fetch them. An
    /// extension that syncs provider permissions also sets the
    /// `provider_permission_sync` capability: without it the core rejects
    /// that mode and never leases its sync or `permissions` jobs.
    fn source_permissions<'a>(
        &'a self,
        _remote: &'a mut Remote<'_>,
        _item: &'a Value,
        _doc: &'a Value,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        None
    }
    /// Environment variables this extension reads when the server starts.
    /// `graphicle serve` refuses to start while a variable that configures an
    /// edition feature (hosted billing or the storefront) is set and no
    /// installed extension reads it, rather than silently ignoring it.
    fn environment(&self) -> &'static [&'static str] {
        &[]
    }
    /// Prepare the HTTP server: read configuration, reject an unusable one
    /// with an error, and add background tasks to `context.background`.
    fn serve(&self, _context: &mut ServeContext<'_>) -> Result<()> {
        Ok(())
    }
    /// Add subcommands to the `graphicle` command line. [`install`] rejects a
    /// subcommand (or alias) the core or another extension already has.
    fn cli(&self, command: clap::Command) -> clap::Command {
        command
    }
    /// Run a parsed command line whose subcommand this extension added in
    /// [`Self::cli`]; only those reach it. `None` leaves it to the core.
    fn run_cli<'a>(
        &'a self,
        _matches: &'a clap::ArgMatches,
        _context: &'a CliContext,
    ) -> Option<BoxFuture<'a, Result<()>>> {
        None
    }
    /// JavaScript appended to `/extensions.js`, which the web app loads.
    fn ui_script(&self) -> &'static str {
        ""
    }
    /// Replace a public page of the core, for now `/product`; `None` keeps
    /// the core's page.
    fn page(&self, _path: &str) -> Option<&'static str> {
        None
    }
    /// This extension's tables of work and access history (sign-in
    /// attempts, sessions, audit trails). `deletion.state` leaves them out of
    /// state equality and lists them under `excluded.history`; residue scans
    /// still read them.
    fn history_tables(&self) -> &'static [&'static str] {
        &[]
    }
    /// This extension's tables that deliberately keep naming a deleted
    /// project, such as audit trails; deletion receipts report them as
    /// `retained_by_design` instead of residue.
    fn retained_tables(&self) -> &'static [&'static str] {
        &[]
    }
}

/// A set of extensions, and once installed, its operation table.
pub struct Extensions {
    list: Vec<Box<dyn Extension>>,
    operations: BTreeMap<&'static str, usize>,
    natives: BTreeMap<&'static str, usize>,
    subcommands: BTreeMap<String, usize>,
}

static NONE: Extensions = Extensions::none();
static INSTALLED: OnceLock<Extensions> = OnceLock::new();

/// Install the process's extensions; each program's `main` does this once,
/// through [`crate::cli::main_with`], before anything else runs.
///
/// Panics when extensions are already installed, when an extension claims a
/// core operation other than the [`OVERRIDABLE_OPERATIONS`], when two
/// extensions claim the same operation, when an MCP tool name repeats a core
/// or another extension's tool, when a declared native operation, job kind or
/// subcommand clashes with the core's or another extension's, when a declared
/// job kind is not supported ([`crate::archived`]), or when extensions declare
/// different editions.
pub fn install(extensions: Extensions) {
    let extensions = extensions.indexed();
    if INSTALLED.set(extensions).is_err() {
        panic!("graphicle: extensions are already installed");
    }
}

/// The installed extensions; none until [`install`] runs.
pub fn installed() -> &'static Extensions {
    INSTALLED.get().unwrap_or(&NONE)
}

impl Default for Extensions {
    fn default() -> Self {
        Self::none()
    }
}

impl Extensions {
    /// No extensions: the Core edition.
    pub const fn none() -> Self {
        Self {
            list: Vec::new(),
            operations: BTreeMap::new(),
            natives: BTreeMap::new(),
            subcommands: BTreeMap::new(),
        }
    }
    /// Add an extension; hooks run in the order extensions were added.
    pub fn with(mut self, extension: impl Extension) -> Self {
        self.list.push(Box::new(extension));
        self
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.list.iter().map(|extension| extension.name()).collect()
    }
    /// Build the operation table and check that nothing clashes (see
    /// [`install`]).
    fn indexed(mut self) -> Self {
        if self.list.is_empty() {
            // Core: nothing can clash, and every program start stays cheap.
            return self;
        }
        let core: BTreeSet<&str> = crate::api::CORE_OPERATIONS.iter().copied().collect();
        let mut operations = BTreeMap::new();
        for (index, extension) in self.list.iter().enumerate() {
            for &operation in extension.operations() {
                if core.contains(operation) && !OVERRIDABLE_OPERATIONS.contains(&operation) {
                    panic!(
                        "graphicle: extension {} claims the core operation {operation}",
                        extension.name()
                    );
                }
                if let Some(other) = operations.insert(operation, index) {
                    panic!(
                        "graphicle: extensions {} and {} both claim the operation {operation}",
                        self.list[other].name(),
                        extension.name()
                    );
                }
            }
        }
        for learning in [false, true] {
            for work_state in [false, true] {
                // The core tools (the graph engine's navigation tools among
                // them) and the pull-request tools `graphicle mcp
                // --pull-requests` adds.
                let mut names: BTreeSet<String> = crate::api::core_mcp_tools(learning, work_state)
                    .iter()
                    .chain(&crate::graph_tools::tools(true))
                    .map(|tool| tool.name().to_string())
                    .collect();
                for extension in &self.list {
                    for tool in extension.mcp_tools(learning, work_state) {
                        if !names.insert(tool.name().to_string()) {
                            panic!(
                                "graphicle: extension {} repeats the MCP tool {:?}",
                                extension.name(),
                                tool.name()
                            );
                        }
                    }
                }
            }
        }
        let editions: BTreeSet<&str> = self
            .list
            .iter()
            .filter_map(|extension| extension.edition())
            .map(Edition::name)
            .collect();
        if editions.len() > 1 {
            panic!("graphicle: the extensions declare different editions: {editions:?}");
        }
        let mut kinds = BTreeMap::new();
        for (index, extension) in self.list.iter().enumerate() {
            for kind in extension.job_kinds() {
                if crate::jobs::core_kind(kind.name) {
                    panic!(
                        "graphicle: extension {} declares the core job kind {}",
                        extension.name(),
                        kind.name
                    );
                }
                // No edition runs an unsupported kind (crate::archived).
                if let Some(archived) = crate::archived::kind(kind.name) {
                    panic!(
                        "graphicle: extension {} declares the unsupported job kind {} ({})",
                        extension.name(),
                        kind.name,
                        archived.listing()
                    );
                }
                if let Some(other) = kinds.insert(kind.name, index) {
                    panic!(
                        "graphicle: extensions {} and {} both declare the job kind {}",
                        self.list[other].name(),
                        extension.name(),
                        kind.name
                    );
                }
            }
        }
        let mut natives = BTreeMap::new();
        for (index, extension) in self.list.iter().enumerate() {
            for &operation in extension.native_operations() {
                let Some(name) = operation.strip_prefix("graph.") else {
                    panic!(
                        "graphicle: extension {} declares the native operation {operation} outside the graph. namespace",
                        extension.name()
                    );
                };
                if graphicle_memory::graph::handles(name) {
                    panic!(
                        "graphicle: extension {} declares the memory engine's operation {operation}",
                        extension.name()
                    );
                }
                if let Some(other) = natives.insert(operation, index) {
                    panic!(
                        "graphicle: extensions {} and {} both declare the native operation {operation}",
                        self.list[other].name(),
                        extension.name()
                    );
                }
            }
        }
        // Each extension's subcommands are the ones its `cli` hook appends to
        // the core command line; a name or alias may be used only once.
        let mut command = crate::cli::core_command();
        let mut taken: BTreeMap<String, Option<usize>> = BTreeMap::new();
        for subcommand in command.get_subcommands() {
            for word in std::iter::once(subcommand.get_name()).chain(subcommand.get_all_aliases()) {
                taken.insert(word.to_string(), None);
            }
        }
        let mut subcommands = BTreeMap::new();
        for (index, extension) in self.list.iter().enumerate() {
            let before = command.get_subcommands().count();
            command = extension.cli(command);
            for subcommand in command.get_subcommands().skip(before) {
                for word in
                    std::iter::once(subcommand.get_name()).chain(subcommand.get_all_aliases())
                {
                    if let Some(owner) = taken.insert(word.to_string(), Some(index)) {
                        panic!(
                            "graphicle: extension {} adds the subcommand {word}, which {} already has",
                            extension.name(),
                            owner.map_or("the core", |other| self.list[other].name())
                        );
                    }
                }
                subcommands.insert(subcommand.get_name().to_string(), index);
            }
        }
        self.operations = operations;
        self.natives = natives;
        self.subcommands = subcommands;
        self
    }
    /// The edition the extensions declare; Core when none declares one.
    pub fn edition(&self) -> Edition {
        self.list
            .iter()
            .find_map(|extension| extension.edition())
            .unwrap_or(Edition::Core)
    }
    /// The declared job kind `name`, if an extension runs it.
    pub fn job_kind(&self, name: &str) -> Option<&'static JobKind> {
        self.list
            .iter()
            .flat_map(|extension| extension.job_kinds().iter())
            .find(|kind| kind.name == name)
    }
    /// Every job kind the extensions declare.
    pub fn job_kinds(&self) -> Vec<&'static str> {
        self.list
            .iter()
            .flat_map(|extension| extension.job_kinds().iter().map(|kind| kind.name))
            .collect()
    }
    /// Every environment variable the extensions read when serving.
    pub fn environment(&self) -> Vec<&'static str> {
        self.list
            .iter()
            .flat_map(|extension| extension.environment().iter().copied())
            .collect()
    }
    /// The extension that serves `operation`, if any.
    pub fn operation(&self, operation: &str) -> Option<&dyn Extension> {
        self.operations
            .get(operation)
            .map(|index| self.list[*index].as_ref())
    }
    pub fn capabilities(&self, capabilities: &mut Map<String, Value>) {
        for extension in &self.list {
            extension.capabilities(capabilities);
        }
    }
    pub fn migrate(&self, db: &Connection) -> Result<()> {
        self.list
            .iter()
            .try_for_each(|extension| extension.migrate(db))
    }
    pub fn routes(&self) -> Router<App> {
        self.list.iter().fold(Router::new(), |router, extension| {
            router.merge(extension.routes())
        })
    }
    pub fn mcp_tools(&self, learning: bool, work_state: bool) -> Vec<McpTool> {
        self.list
            .iter()
            .flat_map(|extension| extension.mcp_tools(learning, work_state))
            .collect()
    }
    /// The declared native operation `operation`, served by its extension.
    pub fn native(&self, operation: &str, request: &mut Value) -> Option<Result<Value>> {
        let index = *self.natives.get(operation)?;
        self.list[index].native(operation, request)
    }
    pub fn session_credential(
        &self,
        db: &Connection,
        workspace: &str,
        hash: &str,
    ) -> Result<Option<Value>> {
        for extension in &self.list {
            if let Some(row) = extension.session_credential(db, workspace, hash)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
    /// Every extension must allow the credential.
    pub fn credential_allowed(&self, db: &Connection, credential: &str) -> Result<bool> {
        for extension in &self.list {
            if !extension.credential_allowed(db, credential)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn credential_revoked(
        &self,
        db: &Connection,
        workspace: &str,
        credential: &str,
    ) -> Result<()> {
        self.list
            .iter()
            .try_for_each(|extension| extension.credential_revoked(db, workspace, credential))
    }
    /// The lowest allowance any extension sets.
    pub fn request_allowance(&self, db: &Connection, workspace: &str) -> Result<Option<u64>> {
        let mut lowest: Option<u64> = None;
        for extension in &self.list {
            if let Some(allowance) = extension.request_allowance(db, workspace)? {
                lowest = Some(lowest.map_or(allowance, |n| n.min(allowance)));
            }
        }
        Ok(lowest)
    }
    pub fn usage_details(&self, db: &Connection, workspace: &str, usage: &mut Value) -> Result<()> {
        self.list
            .iter()
            .try_for_each(|extension| extension.usage_details(db, workspace, usage))
    }
    pub fn scrub_backup(&self, db: &Connection) -> Result<()> {
        self.list
            .iter()
            .try_for_each(|extension| extension.scrub_backup(db))
    }
    /// Run a leased job through the first installed extension whose
    /// [`Extension::job`] accepts it; `None` when none does. Operations,
    /// native operations and subcommands reach the extension that declared
    /// them, but jobs are not indexed by kind: each extension must decline the
    /// kinds it does not run. The core's `permissions` kind is leased as soon
    /// as an installed extension reports the `provider_permission_sync`
    /// capability, so that extension must also run it, or a leased
    /// `permissions` job fails (the commercial edition's permissions extension does
    /// both).
    pub fn job<'a>(
        &'a self,
        worker: &'a Worker,
        job: &'a Job,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        self.list
            .iter()
            .find_map(|extension| extension.job(worker, job))
    }
    /// Every job kind the extensions declare as following projections
    /// ([`JobKind::after_projection`]), in installation order.
    pub fn post_projection_kinds(&self) -> Vec<&'static str> {
        self.list
            .iter()
            .flat_map(|extension| extension.job_kinds().iter())
            .filter(|kind| kind.after_projection)
            .map(|kind| kind.name)
            .collect()
    }
    /// The jobs the extensions queued after `episode` of `repository` was
    /// recorded ([`Extension::after_projection`]), in installation order.
    /// Each extension runs inside a savepoint: one that fails has its writes
    /// rolled back and is listed as `{"extension": name, "error": message}`,
    /// and the projection still commits.
    pub fn after_projection(
        &self,
        db: &Connection,
        principal: &Principal,
        repository: &str,
        episode: &str,
    ) -> Vec<Value> {
        let mut queued = Vec::new();
        for extension in &self.list {
            let outcome = db
                .execute_batch("SAVEPOINT after_projection")
                .map_err(Error::from)
                .and_then(|()| extension.after_projection(db, principal, repository, episode));
            match outcome {
                Ok(jobs) => {
                    let _ = db.execute_batch("RELEASE after_projection");
                    queued.extend(jobs);
                }
                Err(error) => {
                    let _ =
                        db.execute_batch("ROLLBACK TO after_projection; RELEASE after_projection");
                    queued.push(
                        serde_json::json!({"extension": extension.name(), "error": error.message}),
                    );
                }
            }
        }
        queued
    }
    /// How many jobs the extensions queued.
    pub fn schedule(&self, store: &Store) -> Result<usize> {
        let mut queued = 0;
        for extension in &self.list {
            queued += extension.schedule(store)?;
        }
        Ok(queued)
    }
    /// The permissions fetched by the first extension that fetches them for
    /// this document; `None` when none does.
    pub async fn source_permissions(
        &self,
        remote: &mut Remote<'_>,
        item: &Value,
        doc: &Value,
    ) -> Option<Result<Value>> {
        // Awaited here, so each extension's borrow of the connection ends
        // before the next one is asked.
        for extension in &self.list {
            if let Some(fetch) = extension.source_permissions(remote, item, doc) {
                return Some(fetch.await);
            }
        }
        None
    }
    pub fn serve(&self, context: &mut ServeContext<'_>) -> Result<()> {
        self.list
            .iter()
            .try_for_each(|extension| extension.serve(context))
    }
    pub fn cli(&self, command: clap::Command) -> clap::Command {
        self.list
            .iter()
            .fold(command, |command, extension| extension.cli(command))
    }
    /// Run a command line whose subcommand an extension added, by that
    /// extension; `None` for the core's own subcommands.
    pub fn run_cli<'a>(
        &'a self,
        matches: &'a clap::ArgMatches,
        context: &'a CliContext,
    ) -> Option<BoxFuture<'a, Result<()>>> {
        let index = *self.subcommands.get(matches.subcommand_name()?)?;
        self.list[index].run_cli(matches, context)
    }
    /// The extensions' scripts, in order; empty without extensions.
    pub fn ui_script(&self) -> String {
        self.list
            .iter()
            .map(|extension| extension.ui_script())
            .filter(|script| !script.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// The first extension's replacement of a public core page.
    pub fn page(&self, path: &str) -> Option<&'static str> {
        self.list.iter().find_map(|extension| extension.page(path))
    }
    /// Every extension's history tables, in order.
    pub fn history_tables(&self) -> Vec<&'static str> {
        self.list
            .iter()
            .flat_map(|extension| extension.history_tables().iter().copied())
            .collect()
    }
    /// Every extension's retained tables, in order.
    pub fn retained_tables(&self) -> Vec<&'static str> {
        self.list
            .iter()
            .flat_map(|extension| extension.retained_tables().iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Claims(&'static [&'static str]);
    impl Extension for Claims {
        fn name(&self) -> &'static str {
            "claims"
        }
        fn operations(&self) -> &'static [&'static str] {
            self.0
        }
    }
    struct Tool(&'static str);
    impl Extension for Tool {
        fn mcp_tools(&self, _learning: bool, _work_state: bool) -> Vec<McpTool> {
            vec![McpTool::new(
                crate::api::tool(self.0, "Probe", json!({}), &[], false),
                "status",
                false,
            )]
        }
    }

    #[test]
    fn no_extensions_serve_nothing() {
        let none = Extensions::none().indexed();
        assert!(none.is_empty());
        assert!(none.operation("status").is_none());
        assert!(none.mcp_tools(true, true).is_empty());
        assert_eq!(none.ui_script(), "");
        assert!(installed().operation("impact.report").is_none());
    }

    #[test]
    fn extensions_may_take_over_only_the_overridable_core_operations() {
        let extensions = Extensions::none()
            .with(Claims(&[
                "impact.report",
                "coverage.upload",
                "graph.impact",
                "probe.new",
            ]))
            .indexed();
        for operation in [
            "impact.report",
            "coverage.upload",
            "graph.impact",
            "probe.new",
        ] {
            assert_eq!(
                extensions.operation(operation).map(|e| e.name()),
                Some("claims")
            );
        }
        assert!(extensions.operation("status").is_none());
    }

    #[test]
    #[should_panic(expected = "claims the core operation status")]
    fn a_core_operation_cannot_be_claimed() {
        let _ = Extensions::none().with(Claims(&["status"])).indexed();
    }

    #[test]
    #[should_panic(expected = "both claim the operation probe.same")]
    fn two_extensions_cannot_claim_one_operation() {
        let _ = Extensions::none()
            .with(Claims(&["probe.same"]))
            .with(Claims(&["probe.same"]))
            .indexed();
    }

    #[test]
    #[should_panic(expected = "repeats the MCP tool \"recall_project\"")]
    fn a_core_mcp_tool_cannot_be_repeated() {
        let _ = Extensions::none().with(Tool("recall_project")).indexed();
    }

    struct Declares {
        edition: Option<Edition>,
        kinds: &'static [JobKind],
        natives: &'static [&'static str],
        subcommand: Option<&'static str>,
    }
    impl Declares {
        const NOTHING: Declares = Declares {
            edition: None,
            kinds: &[],
            natives: &[],
            subcommand: None,
        };
    }
    impl Extension for Declares {
        fn name(&self) -> &'static str {
            "declares"
        }
        fn edition(&self) -> Option<Edition> {
            self.edition
        }
        fn job_kinds(&self) -> &'static [JobKind] {
            self.kinds
        }
        fn native_operations(&self) -> &'static [&'static str] {
            self.natives
        }
        fn cli(&self, command: clap::Command) -> clap::Command {
            match self.subcommand {
                Some(name) => command.subcommand(clap::Command::new(name)),
                None => command,
            }
        }
    }

    #[test]
    fn the_edition_is_declared_not_inferred_from_installing_an_extension() {
        assert_eq!(Extensions::none().edition(), Edition::Core);
        let community = Extensions::none().with(Declares::NOTHING).indexed();
        assert_eq!(community.edition(), Edition::Core);
        let pro = Extensions::none()
            .with(Declares::NOTHING)
            .with(Declares {
                edition: Some(Edition::Pro),
                ..Declares::NOTHING
            })
            .indexed();
        assert_eq!(pro.edition(), Edition::Pro);
        assert_eq!(Edition::Pro.name(), "pro");
        assert_eq!(Edition::Core.name(), "core");
    }

    #[test]
    #[should_panic(expected = "declare different editions")]
    fn extensions_cannot_declare_different_editions() {
        let _ = Extensions::none()
            .with(Declares {
                edition: Some(Edition::Pro),
                ..Declares::NOTHING
            })
            .with(Declares {
                edition: Some(Edition::Core),
                ..Declares::NOTHING
            })
            .indexed();
    }

    #[test]
    fn declared_job_kinds_and_native_operations_are_indexed() {
        const KINDS: &[JobKind] = &[JobKind::new("probe_kind", "read", true)];
        let extensions = Extensions::none()
            .with(Declares {
                kinds: KINDS,
                natives: &["graph.probe_native"],
                subcommand: Some("probe-command"),
                ..Declares::NOTHING
            })
            .indexed();
        assert_eq!(
            extensions.job_kind("probe_kind").map(|k| k.permission),
            Some("read")
        );
        assert!(extensions.job_kind("project").is_none());
        assert_eq!(extensions.job_kinds(), ["probe_kind"]);
        assert_eq!(extensions.natives.get("graph.probe_native"), Some(&0));
        assert_eq!(extensions.subcommands.get("probe-command"), Some(&0));
        assert!(!extensions.subcommands.contains_key("serve"));
    }

    #[test]
    #[should_panic(expected = "declares the core job kind project")]
    fn a_core_job_kind_cannot_be_declared() {
        const KINDS: &[JobKind] = &[JobKind::new("project", "read", false)];
        let _ = Extensions::none()
            .with(Declares {
                kinds: KINDS,
                ..Declares::NOTHING
            })
            .indexed();
    }

    /// No edition runs an unsupported kind (`crate::archived::KINDS`), so no
    /// extension may declare one.
    #[test]
    #[should_panic(expected = "declares the unsupported job kind communities")]
    fn an_archived_job_kind_cannot_be_declared() {
        const KINDS: &[JobKind] = &[JobKind::new("communities", "propose", false)];
        let _ = Extensions::none()
            .with(Declares {
                kinds: KINDS,
                ..Declares::NOTHING
            })
            .indexed();
    }

    #[test]
    #[should_panic(expected = "outside the graph. namespace")]
    fn native_operations_stay_in_the_graph_namespace() {
        let _ = Extensions::none()
            .with(Declares {
                natives: &["rank"],
                ..Declares::NOTHING
            })
            .indexed();
    }

    #[test]
    #[should_panic(expected = "declares the memory engine's operation graph.identity.annotate")]
    fn a_memory_engine_operation_cannot_be_declared() {
        let _ = Extensions::none()
            .with(Declares {
                natives: &["graph.identity.annotate"],
                ..Declares::NOTHING
            })
            .indexed();
    }

    #[test]
    #[should_panic(expected = "both declare the native operation graph.probe_native")]
    fn two_extensions_cannot_declare_one_native_operation() {
        let natives = Declares {
            natives: &["graph.probe_native"],
            ..Declares::NOTHING
        };
        let again = Declares {
            natives: &["graph.probe_native"],
            ..Declares::NOTHING
        };
        let _ = Extensions::none().with(natives).with(again).indexed();
    }

    #[test]
    #[should_panic(expected = "adds the subcommand serve, which the core already has")]
    fn a_core_subcommand_cannot_be_added_again() {
        let _ = Extensions::none()
            .with(Declares {
                subcommand: Some("serve"),
                ..Declares::NOTHING
            })
            .indexed();
    }

    /// `graphicle graph` runs the graph engine in every edition.
    #[test]
    #[should_panic(expected = "adds the subcommand graph, which the core already has")]
    fn the_graph_subcommand_is_the_cores() {
        let _ = Extensions::none()
            .with(Declares {
                subcommand: Some("graph"),
                ..Declares::NOTHING
            })
            .indexed();
    }

    #[test]
    fn extension_mcp_tools_follow_in_order() {
        let extensions = Extensions::none()
            .with(Tool("probe_a"))
            .with(Tool("probe_b"))
            .indexed();
        let names: Vec<String> = extensions
            .mcp_tools(false, false)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, ["probe_a", "probe_b"]);
    }
}
