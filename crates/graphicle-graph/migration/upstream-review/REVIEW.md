# Upstream issue and PR review

## Re-pin to the 0.9.95 release tag (24 October 2026)

The port now follows Graphify **0.9.95**, commit
`92741d162b2094ffc2fed58cfbd8ee823e9da62c` (the v0.9.95 tag, 23 October
2026), read from the GitHub archive of the tag commit (SHA-256
`b4bc34d33b48ca6e9b981809d104eaffcf4e53c9dfffc34bb317294863e4f2ec`). The
ledger accounts for all 1,101 archive files: 96 application modules (0.9.95
adds `graphify/extractors/yaml_config.py`), 89,145 Python source lines and 456
upstream test files. The reference environment is the 0.9.91 one with only
`graphifyy` moved to 0.9.95: its new `pyyaml>=6.0` requirement was already
pinned (PyYAML 6.0.3), and its `tree-sitter-dm <0.26` cap matches the pinned
0.25.1. `since-pinned.json` compares the tag with `v8`: identical.

Graphify 0.9.92 (16 October), 0.9.93 (20 October), 0.9.94 (22 October) and
0.9.95 (23 October) hold 81 release-note items in 147 commits. Every
application change is a native default. Of the 81 items, 69 are adopted (the
engine does what 0.9.95 does), 3 needed no change, 2 are adapted (the same
behaviour in a native form) and 7 are skipped with a reason. Two more
upstream fixes found while porting are adopted.

Extraction and resolution:

- JavaScript and TypeScript: members that differ only by `_`, `$` or `#` are
  separate nodes (#4475, #4479); `this.field` is typed from an assignment in
  any method, `static` and accessors included (#4473, #4482); workspace
  `exports` into `dist/` fall back to `src/` (#4485, #4489); a dangling
  barrel-named import edge is dropped once the import resolves through the
  barrel (#4486, #4488); a member call on an untyped receiver waits for
  receiver-typed resolution (#4598, #4605); a CommonJS `require` binding no
  longer shadows the required export (#4621, #4634).
- Python: module-level class ids that collide with a function or class are
  salted apart (#2810, #4467); member calls on a pytest fixture parameter are
  typed from the fixture's returned class (#4332, #4333); `module.func()`
  binds the module child named exactly `func` (#4577); `TypeAlias`,
  `type X =` and `NewType` are type nodes (#4578); a bare call resolved only
  through a module import stays `INFERRED` (#2945, #4707).
- C#: each overload is a node, the first with the plain id and the others
  salted by their signature, and calls bind by argument count (#4584, #4591);
  `delegate` declarations are types (#4552, #4556).
- Go and Rust declarations that differ only by case are separate nodes
  (#4468); Go `type A = B` (#4579), Rust `type` aliases and `union` items
  (#4581) and C++ namespace-scope `using`/`typedef` (#4580) are type nodes; a
  Rust `use` rooted at a sibling `mod` reaches its declaration (#4564,
  #4567). Kotlin `typealias` (#4553, #4557), Dart `typedef` (#4582) and Swift
  `macro` declarations with `#macro(...)` calls (#4551, #4555) are extracted.
  Metal and CUDA qualifiers mint no sourceless nodes (#4550, #4554), and a
  `.h` with `extern "C"` goes to the C++ extractor (#4526).
- Solidity inherited calls, `super` calls and modifiers resolve through C3
  linearization (#4379, #4384).
- Objective-C: category ids are relative to the repository (#4666, #4642); a
  member call resolves across a header and an implementation in different
  folders (#4673, #4674); `NS_CLOSED_ENUM` is an enum (#4705); a class
  declared in one repository and implemented in another is one definition
  (#4690, #4691); a cross-repository call edge points at the call site
  (#4668, #4669).
- SQL: T-SQL names are unbracketed and procedure bodies record `reads_from`
  and `writes_to` (#4635, #4649); `GO` ends a statement (#4651, #4652); a
  table referenced before its `CREATE TABLE` folds onto it and
  schema-qualified names bind (#4653, #4654); a `CREATE TABLE` the grammar
  cannot parse is recovered from the error node (#4613, #4677).
- Markdown: vault-relative wikilinks resolve (#4559); a bare method link
  needs its class cited (#4609, #4617). Each `package.json` dependency entry
  declares the node its importers resolve to, labelled `name@range` (#4569,
  #4572).
- YAML is code: `.yaml` and `.yml` are read structurally into a file node
  and `contains` edges to their keys, data YAML is declined by design, unsafe
  tags are refused, and output is capped at 1 MiB, 500 pairs and depth 6
  (#2214, #4706). The engine reads YAML with its own `safe_load_all`
  equivalent (Python dict keys, merge keys, non-finite floats).
- Credential-shaped text in rationale labels is redacted (#4566).
- An incremental build stops re-queuing a file declined by design: such
  files are `skipped_sources` and carry `ast_empty` in the manifest (#2543,
  #4672). As upstream, a manifest written before the marker re-queues a
  declined file once more.
- The engine's AST cache schema moves from 9 to 10 (upstream's moves from 17
  to 25), so the first build after upgrading re-extracts every file.

Build, serve, command line, security, hooks, installer, language model,
exports:

- Parallel edges survive graph.json and query output (#4500, #4503); a node's
  confidence folds like an edge's (#4495, #4501, #4529, #4531); a node with a
  source wins an id collision over a sourceless namesake (#4492, #4496); raw
  builds resolve imports through the old-stem alias index (#4670, #4676).
- Serve and query: near misses are reported rather than answered (#3917,
  #4618), an exact name of several nodes is refused (#4511, #4517),
  same-path files of two repositories stay apart in `explain` and `path`
  (#4512), an IPv6 bind accepts `[::1]:port` (#4656).
- Security: `validate_url` refuses hostless URLs, bad or zero ports and
  embedded credentials (#4643, #4644, #4645); the SSRF guard blocks multicast
  (#4708; `migration/IP_POLICY.json` regenerated); Obsidian file-type and
  confidence tags are sanitized (#4709); the Google Workspace sidecar escapes
  YAML fully (#4710); skill installs stage through random exclusive names
  (#4513); a text capture never writes through an entry (#4657).
- Detection: only a valid `.git` is a repository root (#4444, #4464);
  versioned shebang interpreters classify (#4535); an unreadable PDF warns
  once (#4527); binary media counts no words (#4548); an rc file is read as
  `utf-8-sig` (#4538).
- Hooks: post-merge runs on real merges, post-rewrite rebuilds after a
  rebase, cherry-pick or revert, once (#4463, #4476, #4490, #4491), and every
  hook runs from the top level (#3258, #4689).
- Command line: `--version` and `--help` leave installed skills alone
  (#4545); `--help` lists every `--platform` value (#4655).
- LLM: the extraction prompt pins `source_file` (#3377, #2270, #4487), which
  changes the semantic cache key, as upstream; `GRAPHICLE_BACKEND` selects or
  disables the provider (#4361, #4376); salvaged community labels are
  JSON-decoded (#4537).
- `prs` reads StatusContext states (#4533) and keeps a slashed default branch
  whole (#4534); a Maven pom contributes only its own dependencies (#4596);
  `affected` resolves a disambiguated file label (#4698, #4713); the tree
  export draws one root per repository on a merged graph (#4524, #4525).
- Adopted with no change needed: the GraphML writer already wrote
  multigraph edges with `id=key` (#4536); every native hook command already
  quoted a spaced executable (#4662); Rust's file calls already take the
  extended-length form for long Windows cache paths (#4542).
- Adapted: the native post-commit block leaves the sequencer rule to
  `hook launch` (#4463 and the rebase items); the unreadable-PDF warning is
  upstream's message with the native parser's error text (#4527).
- Found while porting, both adopted: the rebuild lock file is removed on
  release (#4273, 0.9.8x), which supersedes the 0.9.70 note below that the
  lock inode stays; post-checkout skips a checkout that keeps HEAD (#2421).
- Found in review, adopted: the installed SKILL.md and the version stamp are
  renamed over their own names, as upstream's `os.replace` does, so a
  symbolic link there is replaced rather than written through (#3286), and
  references/ stages in a 0700 folder (#4513); `update --no-cluster` and the
  GraphML export replace a link at their output as well; a redirect's
  `Location` is checked as `urllib` joins it, so an empty user name is
  refused (#4643).

Skipped, with the reason:

- The scan root shared through `extractors/base.py` (#3666, #3758): a Python
  import-structure refactor with no behaviour change; the engine already
  scopes the scan root per extraction.
- Skill text: code-only `--update` materializing the extraction (#4447,
  #4313, #4456), stray backticks in `-c` blocks (#4461, #4462), Pi
  semantic subagents (#4494, #4502), the exact Neo4j and FalkorDB push URI
  (#4349) and self-reported chunk coverage (#3987, #2225). Graphicle's skill
  runs the native `update`, which does the AST pass and the merge in one
  process, and has none of those blocks.
- The `tree-sitter-dm <0.26` pip cap (#4044): Python packaging. The engine
  pins the crate `tree-sitter-dm =0.25.1`, whose `parser.c` has
  `LANGUAGE_VERSION 15`, the version upstream's lock file pins.

Superseded adaptations: `tools/upstream_fixes.py` drops the #2712/#2716
bracket hunks, because 0.9.94 (#4635) unbrackets T-SQL names upstream and the
engine follows it (labels are `dbo.Alpha`, not `[dbo].[Alpha]`); the
"Adopted changes" row for PR 2716 below describes the earlier behaviour. The
#3020 hunk stays, re-anchored after the recovered-table pre-registration of
#4613. `adaptations.json` drops the #2716 record.

Graphicle's own defaults beyond 0.9.95, outside the parity claim:

- Every reader that builds a graph from a saved graph.json repairs a file
  whose links hold parallel edges under `multigraph: false`, as 0.9.95's MCP
  loader does, so the same links read the same whichever release wrote the
  file (`build::mark_parallel_links`): the MCP server, `affected::load_graph`
  (the memory library, Pro and sessions), the command-line loader (`query`,
  `explain`, `path`, `merge-graphs`), the global graph, `export` (every
  format), `merge-driver` and `paths::load_node_link_graph` (`benchmark`,
  `prs` and the `paths.load_graph` operation). Upstream repairs only in its
  MCP loader. `cluster-only` rebuilds the graph from the file's links, `tree`
  reads them directly and callflow reads them as a multigraph, so none of
  these depends on the flag.
- A graph.json over the 512 MiB cap that the engine wrote itself is read
  whatever its size where the engine updates, serves or queries it
  (`security::check_own_graph_size_cap`: `update`, `watch`, `serve`, `query`,
  `explain`, `path`, `merge-graphs`, `cluster-only` and `label`); a file
  from elsewhere keeps the cap, as do the global graph's inputs, `prs`,
  `benchmark`, the exports and the incremental merge of `extract`. A file
  over the cap that an earlier release wrote has no record and is refused
  too; the error names the one-time rebuild that records it
  (`GRAPHICLE_MAX_GRAPH_BYTES=<N>GB graphicle-graph extract --force`).

Found while re-pinning and corrected: the rebuild read back its own
temporary graph under the size cap, which upstream's `_rebuild_code` does
not do (it caps only the existing graph.json), so on a project whose graph is
over 512 MiB, kubernetes among them, even the first `update` failed.

Writes a symbolic link could redirect: a cloned repository can commit a link
at any path the engine writes. Where upstream 0.9.95 renames over the path
(`os.replace`), the engine replaces the link (`paths::replace_file`: a private
temporary file renamed over the name, the link's target left alone, nothing
created through a dangling link); it follows a link only where upstream does,
except for the deliberate differences marked below.

| Write | Upstream 0.9.95 | Engine |
|---|---|---|
| SKILL.md, every platform (install, `--project`, vscode, the automatic refresh) | temporary file and `os.replace`: link replaced | replaced |
| .graphicle_version | temporary file and `os.replace`; 0600 when new | replaced, same modes |
| references/ | `mkdtemp` (0700) staging and `os.replace`; a link at references/ is unlinked | the same |
| SKILL.md.bak | `shutil.copy2`: copies the file a linked SKILL.md names, writes through a link at SKILL.md.bak | no copy when SKILL.md is a link; the copy is renamed over SKILL.md.bak (deliberate differences) |
| settings `*.graphicle-bak` (Claude, Codex, Gemini, CodeBuddy hooks) | `shutil.copy2`: writes through a link at the backup | renamed over the backup's name (deliberate difference) |
| settings.json, the CLAUDE.md, AGENTS.md, GEMINI.md, CODEBUDDY.md and copilot-instructions sections, rule, steering, command and plugin files, opencode.json | `write_text`: follows (documented for nix and home-manager, #3805) | follows, as upstream |
| graph.json from `update` and hook rebuilds | a temporary graph, then `os_replace` over graph.json: link replaced | replaced |
| graph.json from extract, `cluster-only` and `label` | `write_json_atomic`: resolves and writes through (documented for shared output folders) | follows, as upstream |
| GRAPH_REPORT.md, label and signature sidecars, manifest, analysis, html, tree, callflow, svg, canvas, cypher, wiki and Obsidian vault | `write_text_atomic`, `write_text`, `open('w')`, `savefig`: follow | follow, as upstream |
| graph.graphml | temporary file and `os_replace`: link replaced | replaced |
| cache entries, the stat index and the engine's syntax facts | `mkstemp` and `os.replace`: link replaced | replaced |
| the rebuild's temporary graph | the fixed name `.graph.tmp.json`, written through a link, then moved over graph.json | a random name created exclusively in the output folder, renamed over graph.json, removed if the rebuild stops (deliberate difference) |
| .graphicle_root, the html stale marker | `write_text`, `touch`: follow | follow, as upstream |
| .rebuild.lock | `open('a+')`, then truncate and write the PID: follows | a link is removed and a regular file created; the open never follows a link (`O_NOFOLLOW`) (deliberate difference) |
| git hooks | `write_text`: follows | follows, as upstream (hooks live in .git, which a clone does not carry) |
| .gitattributes (hook install and uninstall) | `write_text`: appends through a link | a linked .gitattributes is neither read nor written, and reported as left alone; writes refuse a link (`O_NOFOLLOW`) (deliberate difference) |
| .graphiclerc, .graphicleignore | never written | never written |

The three deliberate differences beyond upstream close what upstream still
allows: a repository that commits a link at `.graphicle/.rebuild.lock`,
`.graphicle/.graph.tmp.json` or `.gitattributes` would have the file it names
truncated to a PID line, overwritten with graph JSON, or given a merge line by
`update`, a hook rebuild or `hook install`. For .gitattributes, refusing
matches git, which does not read an in-tree .gitattributes that is a symbolic
link (since 2.32); replacing the link would change a tracked file's type and
drop what it named. Test: `compat_0_9_95_cli`
`links_at_the_lock_the_graph_temp_and_gitattributes_are_not_followed`.

Known differences:

- YAML: PyYAML rejects a tab used as separation after a token in more places
  than the engine detects (the engine refuses leading tabs, as PyYAML does);
  neither the upstream tests nor the generated cases have one. A non-finite
  float inside `!!set`, `!!omap` or `!!pairs` makes the engine refuse the
  file, while PyYAML loads it.
- Secret redaction (#4566): the engine's regular expression engine stops a
  search after a fixed amount of backtracking, where Python's `re.sub` has no
  limit, and a search it cannot finish redacts its text whole rather than
  keep a possible secret. The engine searches a docstring or comment one
  line at a time, which gives `re.sub`'s result (no credential shape spans a
  line break), so the limit applies per line: only a single line of tens of
  thousands of characters comes back as `[redacted]` where upstream keeps
  it. Before, a whole text of about 50,000 to 90,000 characters of ordinary
  prose did. Test: `security::tests::a_long_docstring_is_searched_line_by_line`.
- A drifted `"nan"` or `"inf"` confidence gives Python a non-finite score
  (written as `NaN`/`Infinity`); a JSON value cannot hold one, so the engine
  records `null`.
- A salvaged community label holding a lone surrogate escape (`\ud800`) is
  valid for Python's `json` and not for the engine's parser; it stays raw.
- The comparison checks compare the force-directed layout with NetworkX at an
  absolute tolerance of 1e-8 (2e-9 before): on the Linux x86-64 host one
  coordinate of 22 differs by 2.2e-9, with the same values at 0.9.91 and at
  0.9.95.
- Upstream tests that exercise only a Python seam are recorded as not
  applicable in the checks' receipts: the declined-file rebuild test patches
  the private `_DISPATCH` table; the scan-root test reads a module variable;
  `test_cache_long_paths.py` runs only on Windows; `test_serve_http.py` needs
  `mcp` and `starlette`, which the reference environment does not install.

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
