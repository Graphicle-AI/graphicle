# Structured memory recall

`memory.search` retrieves reviewed notes with explicitly supplied names, aliases,
semantic scopes and validity intervals. It runs on the existing SQLite memory
journal and makes no model calls. The typed selection kernel is
`graphicle_memory::recall`; the public MCP tool is `search_project_memory`.

This feature does not infer entities, classify arbitrary documents, or verify
that an assertion follows from a quotation. The response explicitly sets
`semantic_entailment_verified: false`.

## Store and retrieve

Use the existing `note` command with optional `recall` metadata. For example,
after syncing a source containing the quoted sentence:

```json
{
  "title": "Pager rotation",
  "body": "Mira owns the pager.",
  "recall": {
    "name": "pager rotation",
    "aliases": ["duty owner"],
    "scopes": ["project:larch"],
    "state": "active",
    "valid_from": "2026-01-01T00:00:00Z"
  },
  "evidence": [{"id": "file:operations.md", "quote": "Mira owns the pager."}]
}
```

Save the arguments as `note.json`, then use the normal local session:

```sh
graphicle call note --arguments note.json
graphicle call memory.search --arguments search.json
```

Example `search.json`:

```json
{
  "question": "Who is the duty owner?",
  "scopes": ["project:larch"],
  "match_mode": "identity",
  "limit": 20
}
```

HTTP/MCP callers also supply their repository identifier through the normal
authorized API. `note` is a member-reviewed statement; model proposals retain
their existing draft/review workflow even when they contain recall metadata.
An edit that omits `recall` preserves its previous metadata.

## Selection and history

| Field | Meaning |
|---|---|
| `match_mode: "identity"` | Default. Match complete declared names or aliases in the question. There is no implicit lexical/fuzzy fallback. C, C++, and C# remain distinct. |
| `match_mode: "lexical"` | Explicit discovery by positive token overlap, after eligibility filtering. |
| `scopes: null` or omitted | Search all semantic scopes within the authorized repository. |
| `scopes: []` | Search universal notes only. Empty note scopes mean universal within that repository. |
| `as_of` | RFC3339 valid time; intervals are `[valid_from, valid_to)`. Defaults to the current clock. |
| `known_at` | RFC3339 journal time: choose the revision then recorded. Current source permissions, review and freshness still apply. |
| `include_history` | Allow historical/resolved/open-question states that satisfy the requested validity interval. It does not bypass permissions or freshness. |
| `limit` | Default 20; maximum 200. The kernel accepts at most 20,000 records. |

Semantic scopes never grant source access. Eligibility is checked before
ranking, term statistics, ambiguity reporting and indirect source expansion.
Unstructured notes are omitted from `memory.search`; ordinary `context` retains
their existing behavior. For structured notes, `context` accepts
`memory_scopes` and `memory_as_of` and uses the same eligibility gate.

To replace a fact, add a new active note with the same scopes and an explicit
`valid_from`, plus:

```json
{"supersedes": {"memory_id": "the-old-id", "expected_revision": 1}}
```

The service checks the old revision and closes its validity interval in the
same transaction as the new write. A future replacement leaves the previous
fact available until the effective time. It does not resolve contradictions
by ingestion order. Stale, inaccessible or concurrently changed targets fail
the operation without a partial replacement.

## Evidence and limits

Results contain the selected memories, their match diagnostics, current source
excerpts and `memory_support` coverage. An excluded parent memory cannot restore
its citations through a child. Missing or withheld support is reported rather
than invented. A memory identifier belonging to another repository cannot be
overwritten through an upsert.

Current search reuses the batched freshness calculation. Historical journal
queries still revalidate selected revisions individually. Recall uses the
metadata a note supplies; it does not extract entities or scopes on its own.
