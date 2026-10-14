# Upstream issue and PR review

## Re-pin to the 0.9.72 release tag (30 September 2026)

The former pin, `4d01104edf9f5057fbb841523c7cbdb380c7f60c`, was a
pre-release snapshot of `v8`. The v0.9.72 tag and the PyPI release
(graphifyy 0.9.72, uploaded 2026-09-29) are
`1cd9a36c0c57a661d2d2234bc3207e887e1ba104`, 11 commits later. The port is
re-pinned to the tag:

- archive SHA-256 `8a662a26f63bfbf9c1cbf1a76dcdd9a021c2bbc3a9fbcdc0ebb795f5eb209cac`;
- 946 files, 92 application modules and 75,228 Python source lines;
- the reference environment reinstalled from the tag tree.

The seven application changes are native defaults:

- SQL `block` statements (#3886);
- Razor `@functions`;
- Blade `@extends`;
- nested definitions are not imported modules (#3887);
- the `graph.html` Node Info field names and aggregated member counts;
- the Kotlin `line` hoist (#3884, which the port already had; its oracle
  adaptation was removed);
- the refresh of stale skills after an upgrade (#1805), with upstream's skill
  lock file shared by native and Python writers.

The skill refresh is adapted: it refreshes only copies the native installer
wrote.

Focused checks pass against the tag: Blade (including the new
`tests/test_blade_extractor.py`), Razor, SQL, HTML export, install, CLI,
engine and extract.

`since-pinned.json` shows `v8` identical to the tag. The previous comparison
is kept as `since-pinned-4d01104e.json`. The reviewed patches were
regenerated: 10 patches, 16 behaviours.

Upstream PR [#3956](https://github.com/Graphify-Labs/graphify/pull/3956)
offers the label-retry follow-up that `adaptations.json` records for
`llm.py`.

## Re-pin to 0.9.70 (28 September 2026)

The port now pins Graphify **0.9.70**, commit
`4df8d4dcfadee5555203eaba585cc7dec1dee2fb`, from the tagged archive whose
SHA-256 is `d59017f6797c146e781d92d752bd7d60fe0eb425c48effd142977ea2d998621f`.
The ledger accounts for all 939 archive files (92 application modules, 74,687
Python source lines). The isolated reference environment runs graphifyy 0.9.70.

All seven release changes are native defaults:

- Fortran capital-F preprocessing blanks every cpp `#include` line before
  invoking cpp and passes sanitized bytes on stdin; the attacker-controlled
  filename is never an argument. Failed/missing cpp returns the sanitized
  bytes. Rust tests cover absolute, traversing and same-directory includes,
  fallback, macro expansion and line preservation (GHSA-pcc4-rvhr-2pr8).
- The retained Aider/Devin monolith assets use the trusted `.graphify_root` in
  their watch snippets. The native modular skill already invokes
  `graphify-native watch .` without interpolating an agent-substituted path.
- Terraform name/value pair redaction covers `value`, `valueFrom` and
  `value_from`; the formerly post-pin #3787 adaptation is now ordinary pinned
  behavior.
- `RebuildLock` uses Rust's cross-platform `File::lock`/`try_lock`, including
  Windows. A native regression checks a concurrent nonblocking acquisition is
  rejected. Unlike upstream, the lock inode remains in place after release to
  avoid an unlink/recreate race; its PID is replaced at each acquisition.
- C# tuple type traversal visits only each element's `type` field, never the
  element name.
- bare uppercase/underscore JSX component tags produce `calls`; lowercase DOM
  and member/namespaced tags stay unbound.
- absolute Python imports spelling the scan root's ancestor namespace project
  that prefix onto the local root, for modules and PEP 420 namespace dirs.

Focused upstream checks pass: Fortran 15 plus 4 security tests (1 host-cpp
skip), Terraform 41, JSX 8, C# tuple 2, Python import resolution 20, watch 186
(3 platform skips), install/skill 62 (1 skip). `since-pinned.json` records two
documentation/community commits after the tag and no application change.

## Re-pin to 0.9.69 (27 September 2026)

The port is now pinned to **0.9.69**, commit
`4139885a1212956cf69a76946fbde0d181ab85e9` (97 commits after the former pin).
Everything upstream merged by then is ported as default behaviour, and the
`GRAPHIFY_COMPAT` levels are removed. Of the changes adopted below, PRs 3707,
3708, 3711, 3725, 3735, 3741, 3760, 3762, 3817, 3825 and issue 3775 (as PR 3786)
are in the pinned release, so the oracle runs their pinned source unchanged.
`adaptations.json` lists only what upstream has not merged: the PR 3708
missing-id follow-up, issue 3787, the PR 3020/3270/3278/3450/3544/2388/2963
hunks (PR 3095 landed upstream in 0.9.88; its adaptation is retired), the cache/cli native regressions, and one new fix: at 0.9.69 an annotated
Kotlin class property without an explicit type raises `UnboundLocalError`
(#3842 annotation pass reads an unbound `line`), failing the whole file.

`since-pinned.json` now compares the new pin with upstream `v8` (3 commits, CI and issue-template files only; no application change). The former 0.9.65 comparison is kept as `since-pinned-20a20d30.json`.

The review below is the original 23–24 September record against 0.9.65.

Reviewed on 23–24 September 2026 against Graphify's `v8` branch. The application
port remains pinned to **0.9.65**, commit
`20a20d30d8e7eef77675651f0199d87f913bd3e7`. The comparison head is **0.9.66**,
`a5957aa6ef51c9be8d054de9783d25046c187f3f`.

The saved inventory contains **669 open issues, 777 open PRs, 100 recently
closed PRs and 27 commits after the pin**. Metadata screening selected 104
possible correctness/performance candidates; it is not a claim to have reviewed
the implementation of all 1,446 open items. Selected PR bodies, exact head SHAs,
diffs and the comparison snapshot are saved beside this report. No upstream PR
was merged, closed or commented on by this task. Closed PRs with a null
`merged_at` are not presumed unfixed: several changes appear in the release
through separate maintainer commits.

## Adopted changes

| Reference | Native behavior | Regression evidence |
| --- | --- | --- |
| [PR 3760 / issue 3060](https://github.com/Graphify-Labs/graphify/pull/3760) | Unchanged wiki and Obsidian pages retain their bytes and modification times. New wiki pages are written before obsolete generated pages are removed. User notes remain untouched. | Atomic-write, wiki and Obsidian tests in `src/hardening_tests.rs`. |
| [Issue 3041](https://github.com/Graphify-Labs/graphify/issues/3041) | Creating, modifying or deleting the learning sidecar invalidates both pinned and LRU MCP graph contexts. Unchanged contexts reuse their `Arc`. | `learning_sidecar_invalidates_both_pinned_and_lru_contexts`. |
| [PR 3725](https://github.com/Graphify-Labs/graphify/pull/3725) | `get_node` and `get_neighbors` accept `label`, `node_id` or `id`, with matching tool schemas and missing-argument guidance. | `mcp_identifier_aliases_resolve_the_same_node_and_neighbors`. |
| [PR 3708 / issue 3671](https://github.com/Graphify-Labs/graphify/pull/3708) | Partial community-label responses retry only missing IDs, bound recursion, count every request and report structural fallbacks. | Scripted native transport tests, 80 randomized Python/Rust label scenarios, and real CLI calls against a loopback provider. |
| [PR 3741](https://github.com/Graphify-Labs/graphify/pull/3741) | Installer registration refreshes an exact Graphify H1 block instead of matching an incidental word. Existing CRLF text is preserved. | Exact heading, user heading and line-ending fixtures; platform install/uninstall round trips. |
| [PR 3707](https://github.com/Graphify-Labs/graphify/pull/3707) | A stale `.graphify_root` pointing at a missing directory falls back to the current checkout. | `stale_recorded_merge_root_falls_back_to_the_current_checkout`. |
| [PR 3735](https://github.com/Graphify-Labs/graphify/pull/3735) | Shared absolute `GRAPHIFY_OUT` directories record the resolved corpus root for each caller. | Two independent corpus roots in `check_native_workflows.py`. |
| [PR 3711](https://github.com/Graphify-Labs/graphify/pull/3711) | Same-relation collisions preserve stronger confidence, then existing location/score evidence, in either input order. | Native provenance test and randomized complete-graph differential comparisons. |
| [Issue 3775](https://github.com/Graphify-Labs/graphify/issues/3775) | Dedup survivor selection prefers a node with a populated `source_file` over a sourceless stub before comparing content richness, so located non-code declarations are not discarded for semantic stubs. | `located_declaration_beats_richer_sourceless_stub`; `check_dedup.py` deterministic pair plus randomized whole-graph comparisons against the adapted oracle. |
| [PR 3762](https://github.com/Graphify-Labs/graphify/pull/3762) | Terraform attribute redaction recurses into lists and tuples with the parent key, so `configs = [{ password = "x" }]` is redacted like the map form. | `redaction_recurses_into_lists_of_maps`; `check_terraform.py` list(object) fixture and randomized redaction comparisons against the adapted oracle. |
| [PR 3278 / issues 3277, 3217](https://github.com/Graphify-Labs/graphify/pull/3278) | Apex `extends`/`implements` clauses and method return types accept qualified (`Database.Batchable`) and generic (`Map<String, Object>`) types; heritage names resolve to the tail segment, so no namespace node is fabricated and every method is extracted. | `qualified_and_generic_heritage_and_return_types_are_recognised`; `check_apex.py` deterministic clauses plus randomized line mixes against the adapted oracle. |
| [PR 3020 / issue 3019](https://github.com/Graphify-Labs/graphify/pull/3020) | SQL foreign keys written as `REFERENCES "public"."t"` resolve to an explicit unqualified `CREATE TABLE "t"` (drizzle-kit/pg_dump output) instead of a sourceless `public_t` stub; other schemas still stub. Adapted: the pass runs before the bare-name aliases so a bare alias of `"other"."t"` never captures `public.t`. | `default_schema_qualified_reference_binds_to_unqualified_table`; `check_sql.py` deterministic drizzle pair plus randomized schema/target statements against the adapted oracle. |
| [PR 3450 / issue 3411](https://github.com/Graphify-Labs/graphify/pull/3450) | Same-page wikilink anchors (`[[#Setup]]`, `[[notes#Setup]]`) emit heading→heading `references` edges once the page is walked (forward references included) instead of being dropped by the regex; `[[]]` stays silent and cross-page anchors keep referencing the other page. | `same_page_heading_anchors_link_sections_and_page_anchors_keep_the_page`; `check_markdown.py` deterministic notes page plus anchor/empty/alias snippets in the randomized pages against the adapted oracle. |
| [PR 3772](https://github.com/Graphify-Labs/graphify/pull/3772) | Table-escaped alias pipes (`[[notes/Target\|Alias]]`) no longer leave a trailing backslash in the wikilink target, so the link resolves like the unescaped form. | `table_escaped_alias_pipe_is_not_part_of_the_wikilink_target`; `check_markdown.py` escaped-pipe table row (deterministic and randomized) against the adapted oracle. |
| [PR 2963](https://github.com/Graphify-Labs/graphify/pull/2963) | The Rust `Self` keyword is no longer collected as a type reference, so constructors returning `Self` stop wiring every impl onto one sourceless `self` god node. | `self_return_type_does_not_mint_a_sourceless_self_node`; `check_rust.py` deterministic `Self`/`Self::Item`/`SelfLike` impl plus the existing randomized impls against the adapted oracle. |
| [PR 3544](https://github.com/Graphify-Labs/graphify/pull/3544) | Common Lisp `define-condition` forms are handled like `defclass`, so condition hierarchies (`my-error → app-error → error`) keep their `inherits` edges instead of becoming flat data nodes (dispatch-order change only). | `define_condition_keeps_its_superclass_edges`; `check_commonlisp.py` deterministic condition hierarchy (including a bare and a malformed form) plus the existing randomized definer forms against the adapted oracle. |
| [PR 2388](https://github.com/Graphify-Labs/graphify/pull/2388) | The oversized/low-cohesion community re-splits honour `--resolution` instead of silently re-partitioning at 1.0 (byte-identical at the default resolution). | `oversized_community_splits_honour_the_callers_resolution`; `check_cluster.py` now also runs every fixed graph at resolution 0.1 (barbell(15,4) differs: 2 vs 3 communities) plus caveman/ring-of-cliques graphs against the adapted oracle. |
| [PR 3270](https://github.com/Graphify-Labs/graphify/pull/3270) | `.sln` solution folders (type GUID `2150E333-…`) are skipped instead of becoming phantom project nodes with a bare-name `source_file`; the `path == name` heuristic is dropped so same-named real projects are extracted. The superseded pinned test `test_sln_solution_folder_ids_are_relative` is replaced by the PR's two cases. | `solution_folders_are_not_nodes_but_same_named_projects_are`; `check_sln.py` deterministic folder/App/Tools solution with cross-dependencies plus randomized type GUIDs against the adapted oracle. |
| [PR 3095](https://github.com/Graphify-Labs/graphify/pull/3095) | `index.md` rows link to the slug each community/god-node article was written under (`community_slugs`, `god_articles`) rather than a label lookup, so identically labelled communities (`Core.md`/`Core_2.md`) and god nodes (`GameState.md`/`GameState_2.md`) are all reachable from the index (#3032/#3033); `_md_link` delegates to the new `_md_link_to`. | `wiki_index_links_same_labelled_articles_to_their_own_files`; `check_wiki.py` randomized `wiki.index` with partial slug maps plus the PR's deterministic same-labelled case against the adapted oracle. Landed upstream in Graphify 0.9.88; the oracle is now the pinned wiki.py unchanged. |
| [PR 2716 / issue 2712](https://github.com/Graphify-Labs/graphify/pull/2716) | Bracket-delimited T-SQL object names are read verbatim from the source (`[dbo].[Alpha]`, `[Gamma]`, `dbo.[Delta]`) instead of the grammar-mangled `dbo].[Alpha` span, at every `object_reference` read site; ids and normalized keys are unchanged so `references`/`reads_from` still resolve. Narrower than the PR (no regex has_error fallbacks, no label-convention change). | `bracket_delimited_tsql_names_keep_their_brackets_verbatim`; `check_sql.py` deterministic bracketed DDL forms plus bracketed randomized table/target choices against the adapted oracle. |

The small watcher context change associated with
[PR 3714](https://github.com/Graphify-Labs/graphify/pull/3714) also preserves
language/kind/module/name/arity metadata. This does **not** add the PR's Erlang
extractor or claim full parity with 0.9.66's five additional languages.

## Corrections found during qualification

* Rejected semantic extraction could remain in the content cache, making an
  ordinary retry replay the same rejected result. Rejection now invalidates only
  the current content in its file/prompt/mode namespace. A full CLI test retries
  successfully without `--force`.
* Portable cache records containing `$graphify-root$/` could fail their own path
  identity check, particularly through a walked symlink root. The marker is
  interpreted before validation, while unrelated paths remain rejected.
* The reviewed label patch still let a malformed missing-ID retry trigger an
  outer retry of already named IDs or discard their names. Native recovery
  retains successful siblings and propagates non-parse provider failures.
* MCP stdio and HTTP serialization now enforce size limits. Oversized or
  malformed stdio frames are drained so the following request still works.
* Invalid graph edges are checked before endpoint creation, making failed
  additions atomic. Graph metadata and native model boundaries return typed
  errors for invalid input.

## Deliberate limits

[Swift issue 3540](https://github.com/Graphify-Labs/graphify/issues/3540)
remains a grammar limitation of the pinned `tree-sitter-swift 0.7.3`: the
example's `if let … = await pending` is a syntax error at line 9. Since 09-30
the port parses with tree-sitter 0.25.10, the core py-tree-sitter 0.25.2
bundles, so its tree for the example is the same as upstream's node for node,
and both keep `load()` and `SentinelType` (`tests/runtime_parity.rs`). Under
the 0.27 runtime used before, the whole file became one `ERROR` node and
those two were lost, as the saved reproduction records. No speculative
grammar rewrite or regex-based replacement was applied; this work does not
claim every upstream issue is fixed.

A per-file comparison with Python Graphify 0.9.72 on 17 repositories and
upstream's fixture corpus (09-30; the per-file record is not published),
with the adaptations below applied to the reference and the
`PYTHONHASHSEED=0` its CLI sets, leaves two known differences. PDF text comes
from `pdf-extract`, not pypdf, so PDF word counts and text can differ (the
report's corpus word count on django and jq). Command-line progress messages
are the port's own.

The 0.9.66 method-ghost, new-language and additional Go/Cargo feature changes
are outside this pinned application port. Their presence in the comparison
snapshot is not evidence of adoption. Existing deterministic Leiden seeding and
stable ordering already address the nondeterministic-build concern previously
identified as issue 1667.

## Differential reference policy

`tools/upstream_fixes.py` applies only explicitly selected, exact-context hunks
from the saved upstream comparison **in memory**. The original checkout and its
source hashes remain unchanged. The cache correction and label retry follow-up
are separately described in `adaptations.json` and exercised by independent
native tests. Receipts identify these deviations and hash the qualification
harness. No failing comparison is converted into a passing receipt by refreshing
hashes or suppressing a mismatch.
