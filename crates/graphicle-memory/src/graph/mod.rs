//! Shared graph and memory primitives, independent of commercial extensions.
//! Engine handles and graph references share the same resident registry.
use serde_json::{Value, json};
use std::path::PathBuf;

pub use graphicle::api::*;
#[cfg(test)]
use graphicle::serve_text;
pub use graphicle::{Error, Result};
use graphicle::{affected, graph, python_compat, query_terms, record, serve_search, session};

pub mod anchor;
pub mod canonical;
pub mod community_stability;
pub mod cross_repo_imports;
pub mod identity;
pub mod retrieval;
pub mod span;

pub use self::anchor::{
    Anchor, AnchorCandidate, AnchorDecision, AnchorEdge, AnchorOptions, DocumentChunk, anchor,
    anchor_all, anchor_edges, anchor_with_index, attach as attach_anchors, chunk_id,
};
pub use self::canonical::{
    AliasProposal, AliasSet, ApplySummary as AliasApplySummary, CanonicalOptions, Entity, Rung,
    apply as apply_aliases, normalize_name, propose_aliases, revert as revert_alias,
};
pub use self::community_stability::{
    ContextItem, NamedCommunities, Packed, context_order, remap_communities_to_previous,
    sort_context,
};
pub use self::cross_repo_imports::{
    CrossEdge, EdgeEnd, ExportIndex, LinkReport, PackageIdentity, PackageManifest,
    UnresolvedImport, exports as export_index, link as link_imports, no_sources, sources_below,
};
pub use self::identity::{IdentityOptions, IdentitySummary, LogicalIdentity, logical_identities};
pub use self::retrieval::{
    Candidate, Candidates, ExpandOptions, ExpandedEdge, ExpandedNode, Expansion,
    Mode as TraversalMode, expand, lexical_candidates,
};

/// Durable identity and revision annotation (`identity.annotate`): gives every
/// node a repository-scoped `stable_id` that survives content changes, a
/// content `revision` hash, and every edge a `derivation` plus `provenance`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Identity {
    pub options: IdentityOptions,
}

impl Identity {
    pub fn new() -> Self {
        Self::default()
    }
    /// Repository tag mixed into every `stable_id`.
    pub fn repo(mut self, repo: impl Into<String>) -> Self {
        self.options.repo = repo.into();
        self
    }
    /// Directory that `source_file` values are relative to; revisions are
    /// hashed from files below it.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.options.root = Some(root.into());
        self
    }
    /// Prefix removed from `source_file` before it becomes `relpath`.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.options.strip_prefix = Some(prefix.into());
        self
    }
    /// In-memory source text keyed by `relpath`; wins over `root`.
    pub fn source(mut self, relpath: impl Into<String>, text: impl Into<String>) -> Self {
        self.options.sources.insert(relpath.into(), text.into());
        self
    }
    /// Annotate `graph` in place and report what was derived.
    pub fn annotate(&self, graph: &mut Graph) -> Result<IdentitySummary> {
        identity::annotate(graph, &self.options)
    }
}

/// Lexical retrieval over one graph: the trigram index is built once and
/// reused across queries, so a host can fuse candidates with its own dense
/// retrieval and rerank before packing context.
pub struct Retriever<'g> {
    graph: &'g Graph,
    index: LexicalIndex,
}

impl<'g> Retriever<'g> {
    pub fn new(graph: &'g Graph) -> Self {
        Self {
            graph,
            index: LexicalIndex::new(graph),
        }
    }
    /// Ranked lexical hits and the seeds `serve` would traverse from.
    pub fn candidates(&self, query: &str, limit: usize) -> Candidates {
        lexical_candidates(self.graph, &self.index, query, limit)
    }
    /// Expand seed node ids over the context-filtered traversal view.
    pub fn expand<I, S>(&self, seeds: I, options: &ExpandOptions) -> Result<Expansion>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let ids: Vec<Value> = seeds.into_iter().map(|s| json!(s.into())).collect();
        expand(self.graph, &ids, options)
    }
    /// Candidates for `query`, expanded from their seeds in one call.
    pub fn retrieve(
        &self,
        query: &str,
        limit: usize,
        options: &ExpandOptions,
    ) -> Result<(Candidates, Expansion)> {
        let candidates = self.candidates(query, limit);
        let expansion = self.expand(candidates.seeds.iter().cloned(), options)?;
        Ok((candidates, expansion))
    }
}

/// Shared memory-graph operations beyond the pinned upstream engine.
pub fn handles(operation: &str) -> bool {
    matches!(
        operation,
        "identity.annotate" | "retrieval.candidates" | "retrieval.expand"
    ) || operation.starts_with("cross_repo.")
}

/// Dispatch core graph operations under the same panic boundary as the engine.
pub fn dispatch_owned(request: Value) -> Result<Value> {
    let Some(operation) = request
        .get("operation")
        .and_then(Value::as_str)
        .filter(|op| handles(op))
    else {
        return graphicle::dispatch_owned(request);
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
        "identity.annotate" => identity::dispatch(&request),
        "retrieval.candidates" | "retrieval.expand" => retrieval::dispatch(&request),
        _ => cross_repo_imports::dispatch(&request),
    }))
    .unwrap_or_else(|_| {
        Err(Error::Internal(
            "Native operation failed; no result accepted".into(),
        ))
    })
}
