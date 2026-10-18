# Graphicle graph engine

The Graphicle graph engine is a native Rust port of Graphify **0.9.91** (see
[NOTICE](NOTICE)), pinned to the release tag
`10130e79ed9caf8f60d998073e097531eaa1a395`. The application includes the extraction
pipeline, CLI, incremental updates, watcher, MCP server, hooks, assistant
installer, exports and public library API. Python is used only for development
comparisons; native operations do not fall back to the Python application.

The port was made from 0.9.77, and the eleven releases 0.9.78 to 0.9.88 were
ported for 0.1.0. Together they decode source files that are not UTF-8
(UTF-16 or UTF-32 with a byte-order mark exactly, anything else as cp1252 or
latin-1, with a warning naming the file), write graph.json on one line with
its schema and writer versions, make `query`, `path` and `explain` exit 3
when a name is ambiguous and 4 when nothing matches (`explain --all` lists
every connection, `query --seed` starts from given nodes), read a project's
`.graphiclerc` (file extensions mapped to languages), extract EJS templates,
resolve more calls (Python typed receivers, inherited `self`/`super` calls
and property reads, C# and Java receiver chains, PHP `static`/`self`/`parent`
calls), weight `AMBIGUOUS` edges in clustering (`GRAPHICLE_AMBIGUOUS_SCALE`)
and honour git's user-wide excludes. File extractions cached by an earlier
build are made again once (cache schema 8). Graphify Cloud's call to action
is not ported.

The three releases 0.9.89 to 0.9.91 were ported for 0.1.1. They bind member
calls whose receiver type is declared in another repository (Go, PHP,
TypeScript and JavaScript, Kotlin, Objective-C) on `merge-graphs` and
`global add`, fold a Go package type declared across files into one node,
type C# `foreach` variables and keep C# names that differ only by `_` or case
apart, stop generic type parameters from becoming stub nodes, extract
Objective-C `NS_ENUM` and `NS_OPTIONS`, link an interface member to every
implementer, read Jinja templates as documents, install a `post-merge` hook,
add a Pi guard extension and send Ollama requests to `/api/chat`. File
extractions cached by an earlier build are made again once (cache schema 9).
[CHANGELOG.md](../../CHANGELOG.md) lists every change.

0.9.77 keeps an edge's stored direction wherever graph.json is read back, and
adds enum and context-bound structure to several extractors. Changes in that
release tag:

- Loading graph.json stamps each link's stored endpoints as `_src`/`_tgt`, so
  HTML, Cypher, Neo4j and FalkorDB pushes, Obsidian canvas, the watcher's HTML
  refresh and the git merge driver draw `caller -> callee` even when the node
  list puts the callee first (#4066).
- With deduplication off (`--no-dedup`, `dedup=False` in `build`,
  `build_merge` and `build_from_json`), semantic nodes that share a file and a
  label keep their own ids and edges; an AST twin stays canonical.
- An incremental scan of a subfolder ignores manifest entries recorded for
  other folders instead of reporting them as deleted or excluded (#3785).
- JavaScript members assigned onto an exported object (CommonJS or ESM) become
  nodes with their calls (#3778).
- PHP `$this->m()` and `self::m()` calls whose name matches a language
  construct resolve within the caller's own class, never to another class or
  a free function.
- A zero-byte file keeps its node but has no source location (#4041).
- Scala context bounds (`[T: Ordering]`) record type references (#4080).
- Objective-C enums (plain or behind a `typedef`) become types with `case_of`
  members, without phantom nodes for anonymous or bodyless enums; Solidity
  enum values and PowerShell enum members switch from `contains` to
  `case_of`.
- PowerShell `$this.M()` and `[Type]::M()` calls resolve to methods, and a
  member call no longer binds to a free function of the same name (#3992).
- A SQL trigger `executes` the routine it calls when that routine is defined
  in the scanned sources; an undefined routine gets no edge.
- Anonymous Rust `const _` items no longer produce a `_` node that replaced
  the file node.
- Astro masking ends each script with a line separator, so a trailing `//`
  comment cannot hide the next script; byte offsets and line numbers are
  unchanged (#4072).
- The skill runbook now runs its merge step when every semantic chunk is
  cached. The native executable needs no counterpart: it merges cached
  semantic nodes itself.
- New upstream tests pin behaviour the port already had: C++ union member
  functions, C# `this?.M()`, shell `source` forms as imports, cross-language
  JVM inheritance, Kotlin annotation class literals, Pascal valued enum
  literals, a cross-drive partial-extraction warning and `merge-graphs
  --previous` keeping removed nodes removed.

0.9.76 adds type structure for Zig tagged-union variants, C++ `union`
specifiers, VB.NET enum members (now `case_of` rather than `contains`) and
Pascal enumerated types, and closes a git-hook installer path escape
(CWE-22). Changes in the release tag:

- `hook install` refuses a `core.hooksPath` that resolves outside the
  repository (both sides resolved, symlink-safe) and falls back to the in-repo
  `.git/hooks`; in-repo custom hook paths, linked worktrees and submodules
  still work.
- JSON Schema files (`$schema` plus `$defs`, `definitions` or `$id`) are no
  longer walked as config manifests.
- Elixir `import`/`use` call scoping is per module rather than per file.
- A `module.func()` call to a plainly imported external Python module records
  a `calls` edge to that module; shadowed receivers and non-unique bindings
  fail closed.
- Svelte files feed only their `<script>` blocks to the AST pass, with line
  numbers preserved, and PHP language constructs (`isset`, `empty`, `list`,
  `eval`, …) no longer bind to a user method of the same name.
- An unresolved base class whose only same-named definition is in another
  language stays unresolved.
- Dedup keeps unstamped document nodes with the same heading in different
  files apart; same-file twins still merge.
- `path` and the MCP `shortest_path` tool resolve a `path::symbol` or raw node
  id exactly before fuzzy scoring, and refuse an ambiguous endpoint.
- Cross-repo resolver confidence scores snap to the canonical INFERRED rubric.
- Scans skip the installed skill folders and whole-written rule/hook files
  (the engine's own and upstream's).
- Suggested questions are spread across signal types round-robin.
- The PyPI project-URL change has no counterpart in the port.

0.9.75 binds `self`/`this`/`super` member calls to the caller's own class
chain in Python, JavaScript/TypeScript, Swift and Ruby, and lifts
single-implementer interface dispatch into a language-agnostic core that also
dispatches a Swift protocol requirement to its sole conformer. It extracts Zig
error sets, PHP enum cases, Elixir `defmacro`/`defguard` definitions, Rust
`macro_rules!` with local invocations, Scala file-scope annotations, type-alias
right-hand sides and context bounds, resolves Elixir calls through
`import`/`use`, records bash `invokes` edges through non-shell interpreters
and links Dart `part` files and every configurable-import branch. It keeps
recursive calls through C# partial-class merges, routes `path` through
`contains` edges, rebinds the AST cache's stat index per cache root, restores
`merge-graphs --previous` communities by node id, keeps parallel edges in
`cluster-only`/`label`, merges Ollama `num_ctx` into `extra_body`, names the
effective `graph.json` in the hook guard, refuses a dedup-shrunk overwrite
without `--allow-dedup-shrink`, and warns once about a parsed code file with
no symbols.

0.9.74 captures Ruby paren-less self-sends, TypeScript abstract method
signatures, Scala deferred methods, C++ out-of-line member declarations as
methods, Kotlin annotation class literals, Lua colon-method calls, Erlang
local fun references and C# null-conditional calls. It resolves Rust forward
references and filters Rust prelude types (tuple-struct construction becomes a
`references` edge), resolves dotted wikilinks and constructor calls to
same-file annotation stubs, checks real containment in the CLI
output-directory guard, prefers a workspace package's source entry over its
built dist, detects Verilog `.vh` headers and keys carried incremental
hyperedges by `(id, source_file)`.

0.9.73 extracts enum members as nodes with `case_of` edges to their enum in
Rust, Zig, C++ and Scala 3, and resolves Java calls to inherited methods and
`super.method()` to the nearest declaring ancestor (an unknown or ambiguous
ancestor still fails closed). Changes in the release tag:

- Solidity file-level free functions and their calls are extracted, and
  VB.NET type- and module-qualified calls resolve to the target method.
- Astro files feed only their frontmatter and `<script>` blocks to the AST
  pass, with line numbers preserved.
- The surprising-connections cross-repo bonus matches the reason it prints,
  and under `--exclude-hubs` a node whose only neighbours are excluded hubs
  stays with its hub's community.
- `--push` to Neo4j or FalkorDB indexes each label's `id` before the upserts.
- Community labeling keeps the labels it already named when a nested retry
  fails to parse.
- `hook status` reports hooks written by an older release as out of date.
- A virtual-workspace `Cargo.toml` (`[workspace]` without `[package]`) is
  skipped by design rather than reported as producing no nodes.
- `uninstall` keeps a symlinked instructions file (`CLAUDE.md` etc.) and strips
  only the graphicle section; `install` writes through the link as before (the
  port prints no per-file lines, so upstream's real-target wording has no
  counterpart).
- Semantic extraction warns once when a file exceeds the 20,000-character cap.

0.9.72 keeps `docx` tables in their document position and reads tracked
insertions, content controls and text boxes, reserves `index` in the wiki
for its catalog, counts only structural nodes in the report's community
listings, and shows the logo banner on `--help` (upstream's hosted-platform
link is not carried over, as with `install`). Changes in the release tag:

- SQL DDL before a `DO $$ … $$` block is extracted.
- Razor reads `@functions` blocks and Blade links `@extends` layouts.
- A nested definition is no longer taken for an imported module.
- `graph.html`'s Node Info panel shows each node's real fields, and member
  counts in the aggregated view.
- The CLI refreshes installed skills that an upgrade left behind
  (`GRAPHICLE_NO_AUTO_REFRESH=1` turns this off).

0.9.71 extracts SQL `CREATE TRIGGER` statements (linked to the table they
fire on, including procedural bodies the grammar rejects) and Groovy enums,
recognises namespace-qualified R class constructors (`R6::R6Class`) and R6
`self$`/`private$` calls, keeps foreign-platform manifest keys in their own
path syntax, and rewrites only the host of an `x.com` URL for oEmbed. 0.9.70
closed the capital-F Fortran cpp arbitrary-file-read advisory
(GHSA-pcc4-rvhr-2pr8), hardened Aider/Devin watch instructions and Terraform
secret redaction, and added cross-platform rebuild locking, C# named-tuple
filtering, JSX component call edges and nested-namespace Python import
resolution.

The port covers every application module of the pinned release; Graphify's
original Python test files are kept as development references, and fixtures,
documentation and licenses retain their original formats. The comparison
checks run Graphify's own tests against the engine and compare its results
with the pinned release. Against Graphify 0.9.88, on 13 October 2026, every
application module had a passing receipt from them; on 14 October they were
widened to run more of the upstream `tests/test_*.py` files. Against 0.9.91
they ran on Linux on 15 October 2026, where the modules whose checks need
nothing that host lacks have a passing receipt; the full run on macOS is still
to be made. The repository's `docs/benchmarks.md` gives their results and
limits, its `docs/evidence/upstream-tests-known-differences.md` lists the
upstream tests that fail against the engine, with the reason for each, and its
`docs/evidence/upstream-tests-not-run.md` lists the test files the checks do
not run, with the reason for each.
[migration/STATUS.md](migration/STATUS.md), [AUDIT.json](migration/AUDIT.json)
and the [file ledger](migration/FILES.json) record the 15 October run against
0.9.91: the ledger lists every file of the pinned Graphify 0.9.91 archive with
its SHA-256 and marks each application module that has a passing receipt as
verified. The repository's `docs/evidence/` folder summarizes the 13 October
qualification against 0.9.88 and the 14 October run of the widened checks.

## Build and run

From the repository root, with Rust 1.94.1 or newer:

```sh
(cd crates/graphicle-graph && cargo build --locked --release -p graphicle-graph)
crates/graphicle-graph/target/release/graphicle-graph --help
crates/graphicle-graph/target/release/graphicle-graph extract /path/to/project --code-only
crates/graphicle-graph/target/release/graphicle-graph update /path/to/project
crates/graphicle-graph/target/release/graphicle-graph watch /path/to/project
crates/graphicle-graph/target/release/graphicle-graph serve /path/to/graph.json
# or install the executable:
cargo install --locked --path crates/graphicle-graph/cli
```

An installed Graphicle puts this executable beside its `graphicle` program,
and `graphicle graph <command>` runs it with the same arguments:
`graphicle graph update /path/to/project` is
`graphicle-graph update /path/to/project`, and `graphicle graph` alone prints
the engine's help.

The executable is the workspace package `cli/` (`graphicle-graph`). It pins
the tree-sitter 0.25.10 runtime, the one Python Graphify parses with, so
`cargo install` gets that runtime with or without the lock file. The library
(`graphicle`) keeps accepting newer runtimes for crates that embed it.

`serve` defaults to MCP stdio; `--transport streamable-http` selects HTTP with
host, port, API-key, session and path options. `install` and `uninstall`
configure supported assistant platforms. `--help` lists the commands that
come from Graphify 0.9.91, including labeling, query and navigation, global
graphs, merge workflows and exports; the additions below are documented here
and in the repository's README.

### Features

The default Cargo features enable Chinese tokenization, the precise
JavaScript/TypeScript pass (`precise-js`) and the ten long-tail grammars
(`grammars-extra`). Three features are opt-in, so a default build stays lean:

- `bedrock`: the Amazon Bedrock backend for semantic extraction (the AWS SDK).
- `transcription`: native Whisper transcription (whisper.cpp, which needs
  CMake to build, and model weights and FFmpeg to run).
- `postgres`: the libpq catalog client (`extract --postgres DSN`).

Without `bedrock` and `transcription`, the `bedrock` backend and transcription
report that the build omits them. The `info` operation reports the enabled
build features. External integrations still need their own credentials and
services: URL media download needs yt-dlp, and the optional Claude CLI backend
needs that program installed.

`--no-default-features --features precise-js` builds the core engine: no
Bedrock SDK, jieba or whisper.cpp, and the ten long-tail grammars (Verilog,
OCaml, Fortran, Julia, Objective-C, Elixir, PowerShell, Zig, DM, Common Lisp)
load from grammar packs instead of being linked. `tools/build_grammar_packs.py
--out DIR` compiles the packs from the same pinned grammar crates, and
`graphicle-graph grammars` shows where each grammar comes from. Packs are found
through `GRAPHICLE_GRAMMAR_DIR`, `~/.graphicle-graph/grammars` or `grammars/`
next to the binary. A pack whose grammar has different node-kind or
parse-state counts is refused. Loading a pack runs its code, so only install
packs you built or trust. A pack is checked before it is loaded: its SHA-256
must be one the binary was built with (`GRAPHICLE_PACK_DIGESTS="zig=<sha256>,..."`
at build time) or, when it has none, the one `tools/build_grammar_packs.py`
recorded in the `manifest.json` next to it. The manifest check catches a
corrupted or swapped pack, not a rewritten manifest;
`GRAPHICLE_GRAMMAR_UNVERIFIED=1` skips it.

## Beyond 0.9.91

The parity claim rests on the comparison checks: they compare graph content
with the pinned upstream reference, with the reviewed upstream fixes of
[REVIEW.md](migration/upstream-review/REVIEW.md) applied to it, and they run
the reference's own tests against the engine (see above; the repository's
`docs/benchmarks.md` has their results). That page's
[parity section](../../docs/benchmarks.md#parity-with-graphify) also
records a known difference: on gohugoio/hugo v0.167.0, the graph after
`graphicle graph update` differs from Graphify 0.9.88's in the stored front
matter of one Markdown page, because the engine's YAML parser rejects a list
that the page closes at the start of a line. Folder, environment-variable,
command and installed-file names follow a documented translation table
(`tools/naming.py`), which the checks apply. The first two items below are on
by default and change no output; the rest are opt-in, and the parity claim
does not cover them.

- **Saved query snapshots.** `query`, `explain`, `path`, `affected`,
  `god-nodes` and the MCP server save the loaded graph and its search index
  to `.graphicle/cache/query/*.snap`. Later loads read the snapshot while
  `graph.json` is unchanged (same size, modification time and inode) and the
  binary is the same build; anything else falls back to `graph.json`.
  Answers are identical either way. `update` and watch rebuilds write the
  snapshot as they finish; a cold `extract` does not.
  `GRAPHICLE_QUERY_SNAPSHOT=0` turns snapshots off, and
  `GRAPHICLE_QUERY_TIMING=1` prints the phases.
- **Syntax facts kept across runs.** Cross-file resolution reads each file's
  imports, bindings and member calls. For Python, Java and PHP these facts are
  saved under `.graphicle/cache/syntax/` beside the AST cache entry they belong
  to, so an `update` or a warm run replays them instead of parsing unchanged
  files again. TypeScript's come from the extractor's own parse. The graph is
  the same either way.
- **Commands answered by a running server.** `serve --socket PATH` keeps
  graphs loaded. With `GRAPHICLE_SOCKET=PATH` set, `query`, `explain`, `path`,
  `affected`, `god-nodes` and `retrieve` ask it and print exactly what they
  would print locally.
  - The socket is private to its user: mode 0600, and the peer's uid is
    checked. There is no other authentication, so keep the path in a
    directory only you can write.
  - The server declines when the engine's own `GRAPHICLE_*` settings (an
    explicit list; other programs' `GRAPHICLE_*` variables are never sent) or
    `HOME` differ from the client's, and the command then runs locally.
  - `GRAPHICLE_SOCKET_DEBUG=1` says why a command ran locally.
    `GRAPHICLE_SOCKET_TIMEOUT` (seconds, default 120) bounds a request.
- **Live watch.** `serve <graph.json> --watch [--root DIR]` keeps the graph
  resident, watches the project with FSEvents/inotify and applies each saved
  file to the in-memory graph and search index. The exact incremental rebuild
  of `.graphicle/` follows in the background and is reloaded when it lands,
  so `graph.json` stays the source of truth. `GRAPHICLE_WATCH_TIMING=1`
  prints the phases of every update; `GRAPHICLE_PROFILE=1` on `extract`,
  `update` and `watch` adds wall time, CPU time, cores used and memory per
  stage, pass and resolver on stderr.
- **Precise call resolution.** `--precise` (or `GRAPHICLE_PRECISE=1`) on
  `extract`, `update`, `watch` and `serve --watch` adds call edges that name
  matching, the upstream default, misses:
  - Python: a PyCG-style points-to analysis. It covers lexical scopes,
    imports (including `from m import *` re-exports), parameters, returns,
    attributes, class hierarchy, `super()`, decorators and containers.
  - JavaScript/TypeScript: scope-resolved bindings from oxc and module
    resolution through oxc_resolver (tsconfig `paths`, package `exports`,
    workspace links). It follows re-exports, `this.`/`super.` methods,
    static calls, `new` and receivers typed by annotation or construction.
  - Any language with a SCIP index: call edges from the indexes named by
    `GRAPHICLE_SCIP` or found at `<root>/index.scip` (scip-python,
    scip-typescript, scip-go and `rust-analyzer scip`, for Rust).
  - Dart: imports resolved to files.

  Each added edge records how it was found in `resolution` (`points_to`,
  `super_call`, `oxc_binding`, `oxc_type`, `scip`, `dart_import`) or
  `receiver_evidence`, and every call edge gets an `evidence` level: `name`
  (name matching), `binding` (a resolved import, scope or points-to flow),
  `type` (the receiver's type or the class hierarchy), `indexer` (a SCIP index
  has the call) or `runtime` (seen in a trace named by `GRAPHICLE_TRACE`, the
  format `bench/accuracy/trace_calls.py` writes). `--min-evidence LEVEL` (or
  `GRAPHICLE_MIN_EVIDENCE`) on `query`, `explain`, `path`, `affected`,
  `retrieve` and `serve` leaves out call edges below a level, trading recall
  for precision. `GRAPHICLE_PRECISE_TIMING=1` prints the phases.
- **Documentation pages.** `extract --docs` extracts Markdown and
  reStructuredText pages without an LLM (Sphinx `.txt` sources too) and links
  their mentions of code to the single definition they name: Sphinx roles such
  as `:func:`, object directives, Markdown code spans, and reStructuredText
  literals shaped like code (a dotted path, a call or a CamelCase name).
  Bare-word literals are not linked: in Sphinx prose they are mostly
  parameters, settings and HTTP headers. The choice is kept for `update` and
  `watch` in `.graphicle/.graphicle_build.json` (remove its `docs` key to
  stop). In a full extract `--docs` also keeps those pages away from the LLM.
- **HTTP routes.** `extract --routes` adds a `route` node per declared route
  (FastAPI and Flask decorators, `add_api_route`/`add_url_rule`, Django
  `path()`/`re_path()`, Express-style `app.get("/p", handler)`), a `handles`
  edge from each handler and an `http_calls` edge from each function that
  requests a literal path the route matches (test clients,
  `requests`/`httpx`, `fetch`, `axios`). Router mounts and supported prefix
  constants are followed across files; unknown prefixes stay explicit, and a
  route whose path comes from a variable is not found. These edges do not
  prove runtime reachability. Like `--docs`, the choice is kept for `update`
  and `watch` (the `routes` key).
- **LLM cost estimate.** `extract --dry-run` reports how many files, LLM calls
  and input tokens a full extract would send, and the input price for
  `--backend`, without calling the LLM, writing a graph or saving `--docs`,
  `--routes` and `--exclude` for later builds (only the file-stat cache is
  written). Output tokens are not estimated.
- **Agent retrieval.** `retrieve "<question>" [--budget N] [--json]` answers
  with cited source evidence (`path:Lstart-Lend`, what matched and why) within
  a token budget. Files rank by the BM25 of the whole file plus half the score
  of their best definitions (the name scoring `query` uses, BM25 over each
  definition, and one hop along call, reference and containment edges). Test
  files rank lower unless `--include-tests` is given. `--json --explain` adds
  the parts of every file's score. The same graph and question give the same
  answer on every run. `serve --retrieval` (or `GRAPHICLE_RETRIEVAL=1`) adds
  the same answer as the MCP tool `retrieve_context` (with `include_tests`).

### Evaluation tools

`bench/accuracy/oracle_eval.py` scores `calls` edges against independent
oracles: the call edges in a SCIP index from a compiler-grade indexer, and the
calls observed while a test suite runs (`trace_calls.py`, recall only).
`bench/accuracy/pycg_eval.py` runs the PyCG micro-benchmark, and
`bench/retrieval/localize.py` checks whether `retrieve` puts a file that a
SWE-bench Lite fix changed among its first files. No results of these tools are
published with this release.

## Native integration

The memory library ([crates/graphicle-memory](../graphicle-memory/README.md), the `graphicle-memory` crate)
links this engine, and its `graph.*` operations call it directly.
`graphicle-graph --jsonl` exposes the same JSON boundary without the prefix;
piped no-argument JSON lines remain supported for existing clients:

```json
{"operation":"ids","values":["Große","src/session.py"],"parts":[["src","Session"]]}
```

Rust callers can use the public functions directly with borrowed graph
references and named options. `Error`/`ErrorKind` preserve typed failures and
underlying I/O, parser and provider causes. `model::GraphDocument`,
`NodeRecord`, `EdgeRecord`, `NodeId` and `SourceSpan` provide validated native
records and move attributes into the graph. The legacy NetworkX adapter
retains flexible IDs and metadata for compatibility. See
[tests/public_api.rs](tests/public_api.rs).

Use `dispatch_owned(request)` or `Graph::from_node_link_owned(document, "links")`
when the parsed JSON can be consumed. The owned loading path moves attribute
maps and identifiers into the graph; borrowed callers can continue to use
`dispatch(&request)` and `Graph::from_node_link(&document, "links")`.

JSON dispatch catches Rust unwinding panics. The shared JSON-lines transport
bounds input and output at 64 MiB, drains oversized frames and treats closed
output pipes as normal termination. These boundaries do not claim that every
internal algorithm is panic-free.

## Verification

The comparison suite binds every receipt to an immutable binary, the complete
Rust source set and the comparison tools. It needs the pinned Graphify 0.9.91
source that `migration/FILES.json` names (its archive and SHA-256), unpacked
under `reference/`, and a Python 3.12 environment in `.reference-venv` built
from `tools/reference-requirements.txt` and `tools/comparison-requirements.txt`.
From this folder:

```sh
.reference-venv/bin/python tools/verify_rust.py
.reference-venv/bin/python tools/qualify_all.py --profile release --jobs 2
.reference-venv/bin/python tools/record_qualification.py migration/qualification/run-TIMESTAMP/results.json
.reference-venv/bin/python tools/report_migration.py
```

`verify_rust.py` runs formatting, strict Clippy and the native tests of the
graph engine, the memory library and the runtime; the last two compile the
grammar pack's parser sources, which `python3 ../graphicle/tools/prepare_parser_sources.py`
prepares once. `record_qualification.py` requires a passing full suite and a
current receipt for every application module before it records completion.

[HARDENING.md](migration/HARDENING.md) records the initial hardening and native
regressions, and the [upstream review](migration/upstream-review/REVIEW.md) the
selected upstream fixes and remaining limitations, including the pinned Swift
grammar issue. Explicit oracle adaptations are hashed and recorded in
`migration/upstream-review/adaptations.json`, among them fixes upstream has not
merged (for example the Kotlin annotation crash on untyped class properties).
0.9.91 behaviour is the only behaviour, with no opt-out.

## Notices

Original and dependency notices are retained in `licenses/` and
[NOTICE.md](NOTICE.md). The Python grammar is `tree-sitter-python` 0.25.0,
vendored in `vendor/tree-sitter-python/` with one scanner fix from
`tree-sitter-language-pack`: its state serializer wrote one byte past its
1,024-byte buffer on a deeply indented file, and the engine aborted.
