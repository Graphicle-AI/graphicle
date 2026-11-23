//! The memory namespace a request may read and write.
//!
//! A credential made with a `memory_namespace` reads the shared project
//! (graph, sources, wiki) as any other credential does, and reads and writes
//! only its own namespace's memory: never shared (NULL) memory, never another
//! namespace's. A credential without one keeps the shared, reviewed memory of
//! earlier releases.
//!
//! **Where the guarantee lives.** A [`MemoryScope`] cannot be built outside
//! [`crate::store`]: its only constructors are `pub(in crate::store)`, and
//! [`crate::store::Principal`] (whose fields are private too) is the only
//! value that hands one out, and only for the credential that authenticated.
//! Every memory helper (`memory::list_of_scoped`, `memory::save_scoped`,
//! work state, claims, packs, the job queue...) takes `&MemoryScope`, never a
//! namespace string, so no code path can ask a helper for "namespace X" on
//! behalf of a caller that is not X.
//!
//! Neither of these compiles outside `store`:
//!
//! ```compile_fail
//! use graphicle_runtime::memory_scope::{MemoryScope, Namespace};
//! // A scope for a namespace of one's choosing.
//! let forged = MemoryScope::private(Namespace::parse("someone-else").unwrap());
//! ```
//!
//! ```compile_fail
//! use graphicle_runtime::store::Principal;
//! // A principal whose scope is not the credential's.
//! let mut p = Principal::shared("w", "c", "h", "agent");
//! p.scope = p.memory_scope().clone();
//! ```
//!
//! What the type system cannot see is SQL text. A query that names a
//! namespaced table and forgets the predicate compiles. The integration
//! test `memory_namespace::every_sql_statement_on_namespaced_tables_is_scoped`
//! reads every SQL string of this crate and fails on such a statement unless
//! it is on that test's allowlist, each entry with its reason. Bind the scope
//! with [`MemoryScope::SQL_EQUALS`] and [`MemoryScope::bind`]; never
//! interpolate it, and never `COALESCE` it: NULL (shared) and a namespace are
//! different scopes.

use std::fmt;

/// The tables holding something an agent wrote, a model derived from what an
/// agent wrote, or a cache of either, each with the column that ties a row to
/// its project (directly, or through its parent). Each has a nullable
/// `memory_namespace` column (NULL: shared). Owner operations on a namespace
/// (`crate::namespace_owner`) walk this list in order, parents first.
///
/// Not here: what is derived from the project's sources alone (the code
/// graph, the passage index, observer notes and reflections). Every
/// credential reads the sources, so their derivations are shared like them,
/// and each is served only while its source is in the reader's own view.
pub const TABLES: &[(&str, &str)] = &[
    ("repo_memories", "repository_id"),
    ("repo_runs", "repository_id"),
    ("repo_events", "repository_id"),
    ("am_jobs", "repository_id"),
    ("am_call_reservations", "job_id"),
    ("am_model_cache", "repository_id"),
    ("am_model_calls", "repository_id"),
    ("am_document_reads", "repository_id"),
    ("am_work_state", "repository_id"),
    ("am_entities", "repository_id"),
    ("am_claims", "repository_id"),
    ("am_entity_links", "repository_id"),
    ("am_alias_proposals", "repository_id"),
    ("am_claim_events", "repository_id"),
    ("repo_memory_revisions", "memory_id"),
    ("repo_tool_events", "run_id"),
    ("gf_observations", "repository_id"),
    ("gf_mental_models", "repository_id"),
    ("gf_alias_proposals", "repository_id"),
    ("am_query_cache", "repository_id"),
    ("am_vectors", "repository_id"),
];

/// A validated namespace name: 1 to 128 ASCII letters, digits, `.`, `_` or
/// `-`, not only dots. Holding one grants nothing; only a [`MemoryScope`]
/// does.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Namespace(String);

impl Namespace {
    /// Check a name an owner supplies (credential creation, owner
    /// operations). An empty name is refused, not read as NULL: a key made
    /// with `""` must never become a shared key.
    pub fn parse(value: &str) -> Result<Self, NamespaceError> {
        if value.is_empty() {
            return Err(NamespaceError::Empty);
        }
        if value.len() > 128 {
            return Err(NamespaceError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            // Also a folder's name (a namespace's wiki): never `.` or `..`.
            || value.bytes().all(|byte| byte == b'.')
        {
            return Err(NamespaceError::InvalidCharacter);
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a namespace name was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceError {
    Empty,
    TooLong,
    InvalidCharacter,
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => out.write_str("memory namespace must not be empty"),
            Self::TooLong => out.write_str("memory namespace exceeds 128 bytes"),
            Self::InvalidCharacter => out.write_str(
                "memory namespace must consist of ASCII letters, digits, '.', '_' or '-', and not only dots",
            ),
        }
    }
}

impl std::error::Error for NamespaceError {}

/// Whether a namespaced credential also reads shared memory. Reserved for a
/// later "shared plus mine" mode; nothing reads shared memory for a
/// namespaced credential while this is false, and it must stay false until
/// that mode is designed (each read path would need its own review).
pub const SHARED_FALLBACK_ENABLED: bool = false;

/// The memory one authenticated request may read and write. Not
/// constructible outside [`crate::store`]: get it from
/// [`crate::store::Principal::memory_scope`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MemoryScope {
    namespace: Option<Namespace>,
}

impl MemoryScope {
    /// The NULL-safe equality to put in SQL, with [`Self::bind`] as its one
    /// argument. `IS NOT DISTINCT FROM` reads the same in SQLite and
    /// PostgreSQL, and makes NULL match only NULL.
    pub const SQL_EQUALS: &'static str = "memory_namespace IS NOT DISTINCT FROM ?";

    /// Shared memory (a credential without a namespace, the owner).
    pub(in crate::store) const fn shared() -> Self {
        Self { namespace: None }
    }
    /// The namespace of the credential that authenticated.
    pub(in crate::store) fn private(namespace: Namespace) -> Self {
        Self {
            namespace: Some(namespace),
        }
    }
    /// The value [`Self::SQL_EQUALS`] binds: NULL for shared memory.
    pub fn bind(&self) -> Option<&str> {
        self.namespace.as_ref().map(Namespace::as_str)
    }
    /// The namespace to stamp on a passage-index row (`am_vectors`): this
    /// scope's for a memory passage, NULL (shared) for a source passage,
    /// which every credential reads. A scope reads the index with
    /// `(memory_namespace IS NOT DISTINCT FROM ? OR identity NOT LIKE
    /// 'memory:%')` and [`Self::bind`]: the source passages and its own
    /// memory passages. Readers still keep only the identities and
    /// fingerprints of their own authorized candidates.
    pub fn bind_passage(&self, memory: bool) -> Option<&str> {
        if memory { self.bind() } else { None }
    }
    pub fn is_shared(&self) -> bool {
        self.namespace.is_none()
    }
    pub fn is_private(&self) -> bool {
        self.namespace.is_some()
    }
    /// Defence in depth for a row a scoped query already returned; never a
    /// replacement for the SQL predicate.
    pub fn allows(&self, row_namespace: Option<&str>) -> bool {
        self.bind() == row_namespace
    }
}

/// Drop the namespace column from a row a reply will carry. A namespace is
/// the key's, never a property of what the key reads: with every reader
/// calling this, a namespaced key's replies are a shared key's, field for
/// field (the parity test `memory_namespace::a_namespaced_key_sees_what_a_shared_key_sees`).
pub fn conceal(row: &mut serde_json::Value) {
    if let Some(fields) = row.as_object_mut() {
        fields.remove("memory_namespace");
    }
}

/// The index that `stats` reads to find idle namespaces. (The store format
/// the namespace columns require is recorded with them, by
/// [`crate::store::require_namespace_reader`].)
pub fn migrate(db: &crate::db::Connection) -> crate::Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS am_memory_namespaces_recent ON am_memory_namespaces(workspace_id,last_read_at,last_write_at)")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MemoryScope, Namespace, NamespaceError, SHARED_FALLBACK_ENABLED};

    #[test]
    fn names_are_validated_and_never_sql() {
        assert_eq!(
            Namespace::parse("agent_07-A").unwrap().as_str(),
            "agent_07-A"
        );
        assert_eq!(Namespace::parse(""), Err(NamespaceError::Empty));
        assert_eq!(
            Namespace::parse("a b"),
            Err(NamespaceError::InvalidCharacter)
        );
        assert_eq!(
            Namespace::parse("' OR 1=1 --"),
            Err(NamespaceError::InvalidCharacter)
        );
        for dots in [".", ".."] {
            assert_eq!(
                Namespace::parse(dots),
                Err(NamespaceError::InvalidCharacter)
            );
        }
        assert!(Namespace::parse(".a").is_ok());
        assert_eq!(
            Namespace::parse(&"a".repeat(129)),
            Err(NamespaceError::TooLong)
        );
    }

    #[test]
    fn no_scope_sees_another_and_none_falls_back_to_shared() {
        let shared = MemoryScope::shared();
        let a = MemoryScope::private(Namespace::parse("tenant-a").unwrap());
        let b = MemoryScope::private(Namespace::parse("tenant-b").unwrap());
        const { assert!(!SHARED_FALLBACK_ENABLED) };
        assert!(shared.allows(None) && !shared.allows(Some("tenant-a")));
        assert!(a.allows(Some("tenant-a")) && !a.allows(None) && !a.allows(Some("tenant-b")));
        assert!(!b.allows(Some("tenant-a")) && !b.allows(None));
        assert_eq!((shared.bind(), a.bind()), (None, Some("tenant-a")));
        assert_eq!(
            (a.bind_passage(true), a.bind_passage(false)),
            (Some("tenant-a"), None)
        );
        assert_eq!(shared.bind_passage(true), None);
    }
}
