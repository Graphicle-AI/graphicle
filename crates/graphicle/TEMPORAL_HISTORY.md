# Historical graph views

The graph keeps repository-scoped episodes and source revisions. An episode's
recording order is separate from its asserted valid time. Reads use the
existing workspace capability and repository access checks and require read
access.

`graph.as_of` accepts either:

- `as_of`: a retained episode, an episode label, `latest` or `working`. Commit
  SHAs that earlier releases registered with `graph-commit` remain labels of
  their episodes.
- `valid_at`, `known_at` and optional `branch`: RFC3339 timestamps with explicit
  time zones and a branch name. The result is the newest valid assertion that
  was recorded by the knowledge cutoff. Omitted clocks use the current time;
  the default branch is `default`.

These selectors are mutually exclusive. Each projection records its episode on
the `default` branch, valid from the time it was recorded. The service supplies
`recorded_at`; callers cannot backdate knowledge time. Old databases migrate
with the explicit `recording_time_baseline` origin, since their original world
time is unknown.

Accepted aliases and reversions are append-only events, which the `alias.*`
operations of an edition that has them record. Historical canonical views apply the aliases that
were active at the requested clocks without mutating the cached graph.
Acceptance or retraction can include `valid_at`; historical acceptance that was
lost before this migration is not reconstructed.

Historical views also apply the named relations among graph nodes (hyperedges)
that earlier releases recorded: they retain earlier assertions, report missing
members as `unresolved_members`, and bound the hyperedges they return, with
explicit truncation. Recording assertions and hyperedges by hand
(`graph.observe`, `graph.hyperedge`), registering Git commits (`graph-commit`,
`graph.commit`) and walking their ancestry (`graph.ancestry`) are not part
of this release. `parent_episode` is the previous recorded
episode.
