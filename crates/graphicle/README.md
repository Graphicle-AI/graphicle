# graphicle-runtime: the `graphicle` program

This crate builds `graphicle`, the program of Graphicle Core: the command line,
the browser app, the HTTP service, the MCP servers, the job coordinator, source
connectors and the SDKs (`../../sdk/`). It keeps original sources, reviewed knowledge
and answer evidence in one persistent store. Everything runs in Rust; the Mac
launcher and the optional image-recognition helper use Swift (`../../desktop/`).
Python is used for tests and comparisons, not by the installed program.

The repository's [README](../../README.md) and [docs](../../docs/) cover installing
and using Graphicle. This page is the reference for the program's behaviour and
for building it. [CHANGELOG.md](../../CHANGELOG.md) lists the changes of each
release.

## Build from source

Build **from this folder**, so that Cargo reads `.cargo/config.toml`. Its
settings compile the grammar pack's parsers from sources in `.tslp-root`,
at the repository root, which `tools/prepare_parser_sources.py`
prepares once: it downloads the pack's 36.6 MB parser bundle, checks its pinned
SHA-256 and keeps what the build reads (about 540 MB), so builds need no
network. Without that folder, every Cargo command downloads and unpacks the
whole bundle (about 2 GB) into the build folder again and recompiles the
runtime, and nothing builds offline.

```sh
python3 tools/prepare_parser_sources.py   # once per checkout
cargo build --locked --release            # target/release/graphicle
cargo test --locked --all-targets
```

`--features local-inference` adds local ONNX embedding and reranking
([LOCAL_INFERENCE.md](LOCAL_INFERENCE.md)). `graphicle graph` needs the
engine, `graphicle-graph`, which builds from `../graphicle-graph`
([its README](../graphicle-graph/README.md)).

`tools/product_demo.py --serve` runs a demonstration on synthetic code: two
graph revisions, a reviewed note, MCP access and budget enforcement. Its
`--binary` defaults to `GRAPHICLE_BIN`, else this crate's debug build.
`tools/build_app.py --edition core --dmg` builds the macOS app bundle and disk
image; Developer ID signing and notarization need your own Apple identity
(`--sign-identity`, `--notary-profile`). [deploy/](../../deploy/README.md) has a
container image, a Caddy Compose recipe and a systemd unit for one team's
server.

## Editions

This crate builds Graphicle Core, the open-source edition. It links
`graphicle-memory` and `graphicle` and depends on no other edition's code. A
commercial edition for teams is in private preview; other editions add their
operations through the extension registry (`src/extension.rs`).

- Core answers other editions' operations with an explicit error (501 after
  authorization), leaves out their MCP tools, and refuses connectors in
  provider permission mode ("is not part of Graphicle Core"), while it still enforces
  the provider permissions an existing store holds.
- `build.info` reports the edition and its capabilities, and
  `graphicle --version` prints it (`graphicle 0.1.3 (core)`).
- A store keeps every table when either edition opens it, and Core keeps the
  other edition's guarantees on a store it has used (`src/store_compat.rs`).

Extensions add operations, native operations and subcommands under names they
declare, and each name reaches only the extension that declared it. A job of a
kind the core does not run goes to the first installed extension whose job
hook accepts it, so an extension's hook must decline every kind it does not
run itself. An extension may also add connector kinds (`extension::Source`):
the core adds, schedules, stages, shares and enforces their connectors like
its own, and the source only reads its provider through the connector job.

## The browser app and the API

`graphicle start` opens the app at <http://127.0.0.1:8854>: **Overview →
Sources → Graph → Memory → Agents & API → Usage**. Graph citations open the
retained original at the cited episode, and graph views keep their selected
anchor and report truncated node and edge lists.

The versioned HTTP endpoint is
`POST /v1/workspaces/{workspace}/repos/{repository}/commands`; the URL binds
the repository. `/docs` serves the quickstart and `/v1/openapi.json` describes
the transport. The [Python SDK](../../sdk/python/README.md) and the
[TypeScript SDK](../../sdk/typescript/README.md) are source packages in `../../sdk/`.

Owner-set HTTP request caps are enforced atomically across HTTP and MCP.
Sign-in, export, deletion, cancellation and account controls stay available at
the limit. `build.info`, which the web app reads to sign in, is exempt from the
cap and is not counted.

## Connect agents

```sh
graphicle sync /path/to/project
graphicle context "What did we decide about deployment?"
graphicle connect claude-code   # also: cursor, vscode, codex, claude (Claude Desktop)
graphicle connect claude-code --graph   # also: memory drafts and graph guard hooks
graphicle disconnect claude-code        # removes what connect wrote
```

`graphicle setup` does all of this in one step, in the folder it runs in:
it syncs the folder, connects the agent clients it finds (Claude Code, Codex,
Cursor, VS Code, Claude Desktop; asked on a terminal, else `--yes` or
`--clients`), turns on session capture for Claude Code and Codex, adds a
Graphicle section to AGENTS.md (and to CLAUDE.md when it exists), and adds
Git hooks that keep the project fresh. It lists every file it changes, backs
each one up once as `<file>.graphicle-bak`, and changes nothing on a second
run. `graphicle disconnect` undoes it for the folder (`--all`: everywhere),
and `graphicle uninstall` disconnects everywhere and says how to remove the
program. `graphicle connect` without a client connects every client it
finds.

`connect` connects the project of the folder it runs in (or `--project`: an
id, a key or a folder). Claude Code and VS Code get an entry in the folder
that finds the project from the folder the client starts in; Claude Desktop,
Cursor and Codex get an entry named `graphicle-<project name>`. It keeps
unrelated client configuration and refuses to replace a different existing
entry of that name without `--replace`. Codex connections use the installed
client's MCP command. Reload the client afterwards. MCP reads
evidence with that agent's model; retrieval itself does not call another
model. Proposal tools are opt-in with `mcp --allow-learning` (or `connect
--allow-learning`, or `connect --graph`); they never approve their own memory.
[docs/mcp.md](../../docs/mcp.md) lists every tool.

The one server also answers the graph engine's navigation tools under
Graphify's names (`query_graph`, `get_node`, `get_neighbors`, `get_community`,
`god_nodes`, `graph_stats`, `shortest_path`) and its six resources, on the
project's recorded code graph, with the engine's text followed by a short
citations block; `structuredContent` adds the `stable_id`, episode and
checked `file:` range of each node an answer names (with `--tools
essentials`, only for a call that passes `structured: true`). The service's own
statistics tool is `graph_history_stats`. `mcp --tools graph|memory` serves one
half (graph: every tool that reads the code graph), `--set
graph_retrieval=false` fixes a recall switch for the session, and
`--pull-requests` adds the engine's pull-request tools (stdio, in the
project's folder). `connect --graph` implies `--allow-learning` and gives
Claude Code guard hooks (`graphicle hook claude-code guard-search|guard-read`)
that point searches and source reads in a synced folder whose code graph holds
code to those tools and leave other folders to the engine's own guard. Each
agent (a subagent on its own) gets at most `GRAPHICLE_HOOK_NUDGE_CAP` nudges a
session (default 5, the first always; `0` lifts the limit), counted in the data
folder.
`graphicle setup`, and `connect` without a client, set agents up this way;
a plain `connect <client>` after setup keeps setup's entry. `disconnect`
removes the entry and hooks it finds in its record, and puts a file's backup
back when the file is then the same.

`sync` also builds the project's code graph before it returns, with a line of
progress on standard error, and reports it under `graph`. `--background`
builds it in a detached process instead, and `--no-graph` leaves it for later:
`graphicle mcp` builds a waiting graph when it starts, as do `ask`,
`run --wait`, `serve` and the next sync. Retrieval never builds one. While a
new sync waits for its graph, recall reads the previous one and cites only the
files that have not changed since (`code_graph.episode_lag` counts the
others). `graphicle setup` adds Git hooks that refresh the project and its
graph in the background after each commit, merge and checkout. To add one by
hand, put `graphicle sync --background <folder>` in the repository's
`post-commit` hook, naming the folder you synced. Git runs hooks at the
repository's top level, so `.` names the top level: for a project synced from
a subfolder, write that subfolder's path (for example `graphicle sync
--background services/api`), or `sync` would register the top level as
another project.

`ask` (and SDK `answer()`, and the web app's Ask) reads the code graph and
reviewed memory as `context` does when the project's graph holds code;
other projects, such as one conversation's sessions, are answered from
their sources alone. `context.integration` in the answer says which applied.

One question can also span the projects of a workspace, for example a
library and the application that imports it:
`graphicle context "…" --workspace` (HTTP and SDK: `scope: "workspace"`),
or an MCP connection to the whole workspace, `graphicle mcp --workspace` or
`POST /mcp/WORKSPACE_ID`. Each project the access key may read (at most 8, or
`max_projects` up to 32) is searched as it would be alone, and the results are
interleaved within one budget; every excerpt names its project, and the
imports `sync` recorded between the projects are followed one step. Each
searched project's sources stay in memory until the results are interleaved,
so such a search can use as much memory as that many single-project searches.
The workspace connection also has `list_projects`, and its `recall_project`
and `read_project_source` take `repo` to read one project, as its graph tools
do (it serves no resources). A connection to one project keeps recall, source
reads and memory on that project (the SDK's `Tools` likewise, unless built
with `cross_project=True`), but its graph tools and graph-history tools read
another project the key can read when `repo` names it; to keep an agent to
one project, give it its own key and revoke that key's access to the other
projects with `graph.access`.
[docs/mcp.md](../../docs/mcp.md#several-projects-in-one-connection) has the
details. Answers still read one project.

### Code graphs

`graphicle graph` runs the graph engine, `graphicle-graph`, which builds,
updates and queries a project's code and document graph. Every word after
`graph` goes to the engine unchanged, and the engine's input, output and exit
status are those of the command:

```sh
cd /path/to/project
graphicle graph extract . --code-only   # build the graph
graphicle graph update .                # refresh it without a model
graphicle graph query "how does sign-in work?"
graphicle graph --help                  # the engine's commands; also `graphicle graph`
```

It runs the `graphicle-graph` installed next to `graphicle` (a Homebrew or
release-archive install puts both there), else the first one on `PATH`.
Without one, it says how to install it and exits with status 127.

### Coding-agent sessions

`graphicle connect claude-code --hooks` also installs capture hooks in this
folder's `.claude/settings.local.json`; `graphicle connect codex --hooks` adds
them to `~/.codex/hooks.json`, and Codex asks you to review and trust them
first. After each answer, a detached process imports the session's completed
turns into an owner-only **agent sessions** JSONL connector of the synced
project that contains the session's working directory; sessions in other
folders are skipped. At session start the client receives the titles of
current reviewed memories and the latest captured turns, at most 8,000
characters and never drafts. A resumed Claude Code conversation already holds
that context, so its start receives none; new hooks match only `startup`,
`clear` and `compact`. Hooks never block the client and always exit 0.

A turn is one prompt with everything that answered it. The prompts Codex sends
for its own helper threads (naming a task, consolidating its memories,
suggesting what to try) are not a person's and are not captured. Nor are the
instruction, skill and plugin preambles Codex sends ahead of the first prompt,
or a message that is only a `<task-notification>` block (sent when a
background task ends). A turn is stored verbatim except that
`<private>…</private>` spans are dropped, credentials are redacted, tool input
or output over 16 KiB becomes its size and SHA-256, and model reasoning is
never stored; the applied policy is recorded on each document. Captured turns
are ordinary sources: searchable, citable and covered by certified forgetting.

```sh
graphicle sessions import --client claude-code   # or codex; --path, --since-hours, --project
graphicle sessions timeline
graphicle sessions forget --client claude-code --session-id SESSION_ID
```

`sessions forget` signs a deletion receipt and blocks the session's later
turns. Deleting the connector forgets every captured turn, but transcripts
still on disk are captured again while the hooks stay installed; pausing the
connector stops capture. MCP clients get `session_timeline`. With
`mcp --allow-learning` they also get `propose_memory`, which saves a draft only
when every quote appears verbatim in a current source. Such a draft needs
member review before recall uses it, and it is removed when a source it cites
is forgotten or when it was written while forgotten documents were visible.

### Agent work state

`graphicle connect <client> --work-state` (or `mcp --work-state`, or
`serve --mcp-work-state` for the HTTP endpoint) also gives the agent
`work_state_write` and `work_state_read`: its own to-do lists and tracked
working state for the project. A write names a list and only the fields that
changed. With a `task` field it updates that item (usually its `status`:
`todo`, `doing`, `done` or `dropped`, plus details); without one it updates
the list's own state. Entries are append-only and replayed in order, so the
latest value of each key wins and `null` clears it; `done` or `dropped`, in any
case, closes an item or the list. Whatever is still open leads the
session-start context, at most 3,000 characters and labelled as not reviewed,
until it is closed; `graphicle work-state` (with `--list`, `--include-closed`
or `--json`) shows the lists.

Work state is the agent's own note, not reviewed memory: recall and answers
never use it. String values pass the session-capture policy (`<private>` spans
dropped, credentials redacted) before they are stored, control characters are
shown as spaces so a value cannot add lines of its own, and one write holds at
most 2,000 characters of JSON. Writing needs propose permission. Entries are
deleted with their project and covered by its deletion receipt.

The default data folder is `~/Library/Application Support/Graphicle` on macOS
and `$XDG_DATA_HOME/graphicle` or `~/.local/share/graphicle` on Linux.
Override it with `--data-dir`. Keep the workspace access key or the private
`local-session.json` for recovery.

## Bring your sources

Use **Sources → Add documents**, sync a folder, or configure a source in
**Connections**.

| Source | Current coverage |
|---|---|
| Local folders | Incremental content-addressed snapshots, source history, deletion and original binary retention; respects exclusions and `.gitignore` |
| Documents | PDF, DOCX, PPTX, XLS/XLSX/XLSB, ODS/ODT/ODP, EPUB in spine order, HTML and UTF-8 text |
| Mac document helper | Scanned PDF and image OCR through Vision; legacy DOC/RTF through the OS document reader |
| GitHub | Code/text files, issues and issue comments from one selected repository; full returned tree; truncated trees fail explicitly |
| Slack | Selected channels, paginated history and full threads, with user IDs and timestamps |
| Notion | Selected pages and nested blocks in document order; explicit sharing only |
| Google Drive | Selected files or a folder with recursive subfolders; Google Docs, Sheets and Slides export; supported binary documents |
| Jira Cloud | JQL-selected issues, paginated comments, attributed text from Atlassian Document Format and original timestamps; explicitly authorized copies |
| Confluence Cloud | Selected spaces and paginated pages with title, body, author and source dates; explicitly authorized copies |
| JSONL | Atomic snapshot/delta imports with stable IDs, source timestamps and tombstones |
| Coding-agent sessions | Completed Claude Code and Codex turns through capture hooks or `graphicle sessions import`; private spans dropped and credentials redacted before storage |

Binary originals and extracted text have separate hashes. **Read source →
Download original** retrieves the current, authorized original and rejects a
changed version. Text coordinates refer to extracted text, not PDF page
geometry. Table layout, diagrams and handwriting are not guaranteed to survive
conversion.

Limits are explicit and settable: by default 128 MiB per binary file, 8 MiB of
text per document (extracted, connector or uploaded), 512 MiB expanded archive
and 40,000 archive entries, with bounded spreadsheet ranges and a timed
conversion process (the "Limits" section of the configuration
guide lists every limit and its setting). The Mac helper accepts at most 100
PDF pages.
Oversized or unsupported documents return errors instead of silently reporting
complete ingestion; a folder sync lists them under `skipped` with the reason.

The program includes 49 pinned language grammars for declaration, import and
call extraction. A generic grammar does not establish complete compiler
resolution for that language; ambiguity remains visible. Explicit Python
imports, TypeScript named and namespace imports, direct Rust `use` bindings and
named Python/TypeScript package re-exports can resolve calls across files in
the current authorized manifest. TypeScript configuration supports relative
inheritance and ordered path aliases. Shadowed variables, type-only imports,
reassigned module attributes and ambiguous files do not become guessed runtime
calls. Results identify their static basis and keep `runtime_verified: false`.

### Documents linked to code

Documents link to the code they name when they mark it as code:

- Markdown code spans (`` `parse_header` ``, `` `src/wire.py::Parser::read` ``);
- reStructuredText roles, object directives and code-shaped literals
  (`` :func:`parse_header` ``), in `.rst` pages and Sphinx `.txt` pages;
- in connector documents, Jira code marks and Notion code annotations, which
  are recorded beside the text (the text and its hash do not change), and the
  backticks that Confluence, GitHub and Slack text already carries. A JSONL
  import may give a document's inline-code spans as `code_spans`
  (`[[start, end], ...]` byte offsets into its `content`).

A mention links only when exactly one definition has that name and its
qualifiers, as the graph engine resolves Markdown mentions; the document's own
project is searched first, then the other projects of its workspace. Plain
words, builtins and ambiguous names never link. Conversations stored by the
Python SDK's `Memory` facade and captured agent sessions are not read for
links. The links are kept up to date by the workspace's `link` job, which runs
after each graph build and connector sync (`graphicle sync` and `graphicle
mcp` run it) and recomputes only the documents a change can reach: a changed
document, a name it uses that changed, or the file a link points into.
`graphicle call graph.link` runs it on request, and a store opened by a newer
version that links differently queues it once. That job runs with the next
sync that changes a project of the workspace (a sync that finds nothing new
leaves it queued), or at once with `graphicle call graph.link`; until then
a query that follows links counts the documents it has not read in
`doc_links.pending_documents`.

Recall does not follow the links unless asked: by default a packet counts the
links of the windows it selected (`doc_links.status` is `counted`, with
`withheld_repository` and `withheld_connector`) and lifts nothing, because
whether lifted windows help answers has not been measured yet. With
`doc_links: true` (or `graphicle mcp --set doc_links=true` for every
`recall_project` call), recall follows them one hop, for repository and
connector documents alike: a document passage lifts the definitions it names,
and a code window lifts the passages that name its definitions (the windows'
`doc_links` facts say which link). `doc_links: false` reads no link, and also
keeps documents out of the code graph arm and their links to code out of the
property graph. A packet that follows links reports links waiting to be
recomputed in `doc_links.pending` (and `mechanisms.links_pending_projection`)
and documents the `link` job has not read yet in
`doc_links.pending_documents`. `graphicle call graph.doc_links`
lists the documents that name a definition (`stable_id`), the mentions of one
document (`document_id`), or the project's link coverage, counting only what
the caller may read; how a name resolved across projects is shown only to a
caller who may read every project of the workspace.

## Accounts and permissions

Connections accept an OAuth account or a saved access token. The workspace
owner registers a provider OAuth application once in **Connect account →
Application setup**, then authorizes it in the browser. Client credentials and
source/model tokens are encrypted in the local workspace vault. The device
vault key is a private file; this is not an OS Keychain or cloud KMS
integration. Environment-variable credentials remain available for automation.

Teammates use separate expiring keys through **Workspace access**, each with
its own role, optional expiry and repository scope; revoking a key also
cancels its pending jobs. Company sign-in through an identity provider and
provider-permission sync (`permission_mode: provider`) are not part of
Graphicle Core. Core refuses to configure provider sharing and still enforces the
provider permissions a store already holds, withholding a source whose access
check is missing or expired. Notion, Jira and Confluence do not mirror complete
provider ACLs through these connectors; their copies require explicit audience
authorization and default to owner-only access.

Use **Check connection** for a bounded account and access diagnostic without a
model call. Authentication failures withhold the connector snapshot and
invalidate its derivatives. A successful probe is not a complete permission
audit. Atlassian OAuth includes an authorized-site selector; API-token setup
accepts the account email and site URL. SharePoint, Salesforce and other
connectors are not implemented, nor are Jira attachments and Confluence
macros or attachments.

## Answering and memory

Use **Connections → Answering models → Configure model**. Save a model name,
endpoint and optional API credential; ordinary users do not need environment
variables. An endpoint uses HTTPS, or HTTP on loopback. A model server on a
private network that serves no TLS, such as a vLLM host, needs whoever runs the
service to list its exact host in `GRAPHICLE_PROVIDER_HTTP_HOSTS`, and such a
profile takes no credential
([LOCAL_INFERENCE.md](LOCAL_INFERENCE.md#local-openai-compatible-servers)).
Supported protocols are Ollama, OpenAI-compatible Chat Completions and
Responses, Anthropic, Gemini, Azure Chat and Azure Responses. Azure profiles
target an existing deployment. A saved profile does not provision a model or
establish its quality.

Servers without schema-constrained output can use **Prompt only**. Output is
still validated. One unambiguous array wrapper can be restored; missing
semantic fields, invalid dates, unsupported quotes and arbitrary prose are not
repaired into facts.

```sh
graphicle run extract --max-calls 4 --wait
graphicle ask "What changed about deployment?"
graphicle ask "What should the next maintainer know?" --workflow reflect --retain
graphicle jobs
```

`graphicle ask`, the web app's **Ask** button and the Python SDK's `answer()`
answer with a `basic` job under its default contract, grounded-v2: one model
call reads the retrieved excerpts and cites exact quotes. `ask --workflow
reflect`, and in the web app **Read further and save the answer as a memory
draft**, run `reflect` instead, which can search and read further sources
within `--max-calls` (default 4), checks its answer and can keep it as a draft
for review (`--retain`).

| Path | Behaviour |
|---|---|
| `context` / MCP recall | Original source retrieval and current reviewed memory; no answering-model call |
| `basic` | The default answer path: original-source retrieval with lexical and configured semantic retrieval under the grounded-v2 contract; extracted memories are excluded from candidate selection |
| `reflect` | Opt-in bounded search/read/answer loop; each gather can request four complementary searches and four original source windows, followed by a budgeted source check and an optional retained draft |
| `extract` | Contextual factual proposals, with explicit source grounding and review |
| `refresh` | Reviewable narrative deltas |

Answering jobs keep caller-selected ranking and packing options through
follow-up searches. `local_reranker_profile` selects a pinned CPU
cross-encoder; `embedding_model` or its `local_embedding_profile` alias selects
the dense arm. Local scoring shares the job's call and token budget and records
actual usage. For `basic`, `verify_answer: true` adds the source-review pass
when the budget permits; its status is reported, and it is not a proof of
semantic correctness.

Claims support ADD, KEEP, CONFIRM, SUPERSEDE, CORRECT and RETRACT. Original
sources are retained; unrelated facts are preserved. Knowledge time and event
time stay separate, with immutable revision history. Model-created drafts and
entity links require review before normal recall uses them. A valid quotation
does not prove that every conclusion follows from it.

### Memory anchored to code symbols

A memory's quote in a code file is linked to the code symbols (functions,
classes, methods) that contain it, by `stable_id` and `revision`, with the
symbol's path, qualified name, kind and ordinal beside them. Reviewed memory,
retained answers and member notes then go stale per symbol, not per file: an
edit to a linked symbol's lines, from its start up to the next definition (or
the end of the file), withholds the memory (`symbol_changed:<id>`;
`symbol_missing:<id>` when the symbol is gone), and other edits to the file,
such as lines added above the symbol or a method added to its class, leave it
current. A file moved or renamed with its content unchanged is followed. While a
sync waits for its code graph, a memory on a changed file is held back
(`projection_pending`) until the graph is built. A quote in a module header or
outside any symbol, a quote that runs across two or more symbols (unchanged
symbols can still be reordered or pulled apart), a file without code symbols
(prose, sessions, data) and a file named as a whole keep the whole-file rule.
`propose_memory` and `note` take `symbols` (a `stable_id`, or a `path` and
`qualified_name`) and files; the web app's **Write a memory** form has a
symbol picker. `memory.import` re-grounds the links in the target project by
their path, qualified name, kind and ordinal, and marks the ones it cannot
match. Observer notes are not linked to symbols.

**Reuse across clients.** `memory.template` exports a project's reviewed
procedures and decisions as an engagement template (`graphicle.template.v1`).
Each memory is rebuilt from an allow-list of fields (kind, title, body and
state), so no quote, link, dependency, id, date or author leaves the project.
In the title and body, the memory's quotes, the sources and symbols it cites,
the project's and workspace's names, the `terms` the caller lists (client
names, for example), URLs, e-mail addresses, credentials, words with a path
separator, and the project's file names and code-shaped symbol names become
placeholders such as `[quote]` and `[symbol]`; `redactions` counts them. Other
words stay, so read a template before sharing it. `memory.import_pack` imports
the reviewed memories of a `memory.export` or a template into another project
as drafts (other states are skipped and counted). Only kind, title and body
are copied. Each symbol anchor lands on the live symbol of the same path,
qualified name, kind and ordinal, or, when the symbol moved, on the only
symbol of that qualified name and kind elsewhere in the project; each quote
stays evidence only where one of the project's current sources holds it: the
same source, or the file its anchored symbol moved to, inside that symbol.
What cannot be grounded is dropped and counted by reason in the draft's
`data.pack`. Recall metadata is not carried.

Later windows keep the exact original header and continuation coordinates.
Packing merges overlapping intervals, includes bounded adjacent paragraphs and
enforces the serialized evidence-token ceiling across the final answer context.
`semantic_retrieval: false` and `expand_context: false` give lexical-only and
unexpanded-window comparisons. `compact_citations: true` cites each excerpt by
kind, path and line only: its SHA-256, an ID the path already implies and the
continuation lines stay in `sources` but out of the rendered text, and the
tokens they would cost go to evidence. Prompts show at most 200 characters of
a source's title, then `...`; source text, quotes, headers, memory titles and
the returned evidence keep the whole title.

Three opt-in context options support navigable recall.
`pack_order: "chronological"` still spends the budget on hits in rank order
but presents the chosen excerpts by source date, and marks where a source's
text between two of its excerpts is not shown, with the byte range to read.
`shown` lists up to 200 source spans the caller already holds: a hit inside
them is answered by a one-line reference, reported in `already_shown`. Over
MCP, `recall_project` with `skip_shown: true` fills `shown` from the spans
earlier recalls on the same stdio connection returned. `reading_guidance: true`
adds a header line on weighing speakers.

When a dense arm is present (a configured embedding profile), fusion counts it
at weight 0.5; `arm_weights`, for example `{"semantic": 1.0}`, replaces that
default, and the packet reports the weights used. `origin_quota` (per-origin
caps; `true` takes `{"max": 3, "min": 1}`) is opt-in. With a reranker,
`rerank_depth` and `rerank_mode` apply ([LOCAL_INFERENCE.md](LOCAL_INFERENCE.md)).
The cross-encoder is not a default, and it is not recommended for a project
that holds one conversation's history
([why](LOCAL_INFERENCE.md#reranking-one-conversations-history)).

Recall and `basic` also accept `query_variants`: up to eight
`{"text", "weight", "kind"}` objects (text up to 4,000 bytes, weight 0–4 with
default 1, kind `query`, `keyword` or `passage`). Each variant ranks the same
candidates by BM25 as a further arm (`lexical_q1`, `lexical_q2`, …) at its
weight; repeats of one kind merge and add their weights. With an embedding
profile, `query` and `passage` variants also get a dense arm, `semantic_q{i}`.
The benchmark kit's
[expand_queries.py](../../bench/memory/tools/expand_queries.py) asks an
OpenAI-compatible endpoint for a rephrased question, keyword queries and a
short hypothetical answer, and saves them as variants.

Setting `GRAPHICLE_EMBEDDING_CACHE` to a file path lets indexing reuse passage
vectors across stores and workspaces. Entries are keyed by the embedding model
key and the SHA-256 of the exact passage text. A local embedder runs each text
on its own, never in a batch, so a vector does not depend on what was embedded
beside it. Share a cache only within one trust domain: it holds vectors derived
from private text, and a hit shows that the same text was embedded before. On
Unix the database and its sidecar files are restricted to the owner and opened
without following file symlinks. The cache is off by default.

### Observer notes

`observer_notes` jobs write dated notes about each conversation session, each
backed by an exact quote. `basic` with `observer_notes: true` reads the notes
most relevant to the question beside the original excerpts, inside the same
`context_tokens`. `answer_contract: "grounded-v3"` (opt-in) reads every note
that fits `notes_tokens` beside the excerpts, lists dated excerpts in time
order, writes `reading_notes` about each relevant source before answering, and
abstains with `premise_holds: false` when the question presupposes something
memory contradicts or never mentions.

`note_keys` lets notes find the turns they came from: each source passage is
keyed by the text of the notes whose quoted evidence overlaps it, and
`"merge"` (or `true`) adds the keys to the passage's main lexical ranking. The
keys only route; the evidence reads the original passage. It is off by
default.

`observer.context` can give a reader whose prompt is fixed, such as a
benchmark's, the notes in rank order and the original words behind them. All
of its options are off by default: `ranked_tokens` (the notes most relevant to
the question, best first, with their verbatim quotes), `local_embedding_profile`
(dense note ranking), `question_time` (notes from the period the question names
first, with resolved event dates), `standing: true` (the user's standing
instructions and preferences), `outline_items` (opening sentences of related
user turns) and `conflicts: true` (the user's "I've never …" sentence beside
their other sentences on the subject, for a yes/no question about their own
past). No model is called.

**Recommended recipe for a fixed-prompt reader.** Give the reader the plain
excerpts of `context` beside `observer.context` with `ranked_tokens`,
`question_time`, a `local_embedding_profile`, `standing: true`,
`outline_items` and `conflicts: true`. These options stay off by default until
this composition has run end to end through the Rust path; this is
documentation, not a default.

Independent `observer_notes` jobs can set `parallel_sources: true` to overlap
when every running observer has also opted in and their `include` prefixes are
disjoint; `worker_concurrency` (up to 64) and `max_model_jobs` bound the
concurrency. A connector's `observe` policy runs the observer without a
request: after each import into the connector, each sync and otherwise every
minute, the service counts the tokens of the connector's sources that have no
current note, and once they reach `threshold_tokens` (default 30,000) it queues
an `observer_notes` round over those sources as the workspace owner, within the
policy's model, profile and budget, up to `max_rounds` (default 8) per import
or sync.

The answer check uses the same model and is recorded separately; it is not
independent proof. Invalid or failed checking keeps a previously grounded
candidate, reports the failure and prevents its retention. Date-only
statements remain text; typed event times require an explicit timezone rather
than an invented midnight. Relative time needs an actual source clock. Failed
or cancelled jobs have **Retry** in Activity: a retry receives a fresh bounded
budget and keeps previous usage.

### Retrieval defaults

Recall (`context` and MCP recall) and the answering paths turn on three
retrieval options unless a call sets them:

- `stem: true`: Porter-stemmed BM25.
- `speaker: "auto"`: a further ranking over one side's turns. Questions about
  what the assistant said ("what did you recommend") rank its turns; questions
  that only point at an earlier conversation ("our previous chat") rank
  neither side; other questions rank the user's turns. `true` ranks the user's
  turns and `{"keep": [roles]}` the turns of up to eight roles.
- `pack_focus: true`: evidence windows focused on the question's terms. When
  the focused span of a window that starts a document skips its opening
  paragraph (a transcript's date line, for example), the packet quotes that
  paragraph beside the excerpt as `source_context`. An object also turns focus
  on and sets two options: `{"stem": true}` scores paragraphs by Porter stems,
  and `{"context": k}` (0 to 8) keeps the best-matching paragraph, the
  adjoining paragraphs that also hold a rare question term, and `k`
  paragraphs on each side.

An absent or `null` option takes its default, and `false` turns it off. The
packet's `retrieval_defaults` lists the defaults a call received; `{}` means
the call set all three. MCP `recall_project` sets none of the three, so its
calls always use the defaults. Paired, model-free retrieval runs on
development data chose these defaults ([docs/benchmarks.md](../../docs/benchmarks.md));
the answer accuracy of the defaults has not been measured.

### Benchmark kit

The benchmark kit ([bench/memory](../../bench/memory/README.md))
runs this program. Its
[amb/graphicle_provider.py](../../bench/memory/tools/amb/graphicle_provider.py)
registers a `graphicle` provider in an unmodified
[Agent Memory Benchmark](https://github.com/vectorize-io/agent-memory-benchmark)
(AMB) install and runs AMB's own command line, so loaders, prompts, models and
judging are AMB's. [amb/recall.py](../../bench/memory/tools/amb/recall.py)
asks only the retrieval question, with no model API: on the documents AMB's
loaders produce, does the `context` packet hold the gold documents and
evidence turns? [amb/qa.py](../../bench/memory/tools/amb/qa.py) scores
answers on the saved packets of a recall run with AMB's prompts. AMB is
imported at run time, and nothing is copied from it.

```sh
LOCOMO_DATA_PATH=/abs/locomo10.json .venv/bin/python ../../bench/memory/tools/amb/recall.py \
  --amb <amb checkout>/src --dataset locomo --split locomo10 \
  --variants hyb-w05 --budgets 2000 \
  --embedder .models/arctic-embed-s --ort /abs/libonnxruntime.dylib \
  --binary target/release/graphicle --out runs/amb-locomo
```

Dense and reranked variants need a binary built with `--features
local-inference`. The provider's default recipe is `hyb-w05`: stems, the
speaker arm and the dense arm at weight 0.5, with no cross-encoder.

## Optional backends and hosted use

SQLite stays authoritative for permissions, sources, revisions and commits.
Optional adapters add:

- a Neo4j projection with bounded graph traversal and source revalidation of
  returned results;
- Qdrant vector search with scoped opaque IDs, current source hashes and
  authorization checks (local cosine search remains available);
- inference dispatch over a Redis-protocol queue, with leases, cancellation,
  bounded messages and recovery of abandoned pending work
  (`graphicle redis-worker` runs a consumer on another machine). The
  coordinator authorizes and commits; remote workers do not receive workspace
  access keys. Uncertain paid model usage is never retried automatically.

These are integrations with selected APIs, not complete SDK compatibility.
Source changes, permission loss, connector or project deletion and backend
reconfiguration queue durable, scoped removal of old external copies, and
**Connections → External index maintenance** shows pending work.

For one team's server behind your own TLS reverse proxy:

```sh
graphicle --data-dir /private/graphicle-store init --name "Team"
graphicle --data-dir /private/graphicle-store serve --port 8854 \
  --public-origin https://memory.example.com
```

Keep the service on loopback and pass the configured Host through the proxy.
The public origin controls browser origin checks and OAuth redirects;
untrusted forwarded headers cannot select a redirect host. Hosted mode
disables public workspace creation. The [deployment recipe](../../deploy/README.md)
has the details. For HTTP MCP use `POST /mcp/WORKSPACE_ID/PROJECT_ID` (or
`POST /mcp/WORKSPACE_ID` for every project of the workspace) with
`Authorization: Bearer …`; never put credentials in URLs.

## Maintenance

```sh
graphicle backup /safe/private/path/graphicle.sqlite3
graphicle --data-dir /new/restore/directory restore /safe/private/path/graphicle.sqlite3
graphicle offboard --project PROJECT                       # the plan; nothing changes
graphicle offboard --project PROJECT --apply PLAN_HASH     # carry out that plan
```

`graphicle backup` without a path writes to the data folder's `backups/`
folder. `graphicle offboard` removes one project from the store (the
certified `project.delete`, PR scratch copies included), its graph cache and
its copies in `backups/`, where each backup keeps its other projects and
records its own receipt; the project's receipt lists the backups and files
removed. It touches nothing outside the data folder: links are refused, and
backups written elsewhere, the project's `.graphicle/` folder, what `setup`
added to its folders (`graphicle disconnect`) and memories `memory.import_pack`
copied into other projects are named as not covered. The result is
`verified` only when every deletion checks out and no path was refused.

Portable backups omit provider secrets and the device key, and pause
connections and pending external cleanup that need reauthorization. Reconnect
providers and explicitly resume cleanup after restore. Source snapshots,
reviewed memory and history remain available. Access keys are not recovered
from their database hashes. `connector.delete`, `project.delete` and
`storage.prune` remove their tracked local derivatives while keeping required
source versions; separate backups and external services have their own
retention.

`graphicle update` verifies an Ed25519-signed, expiring release manifest, its
release sequence, the artifact's length and SHA-256 before downloading, and
takes only the installer of the running edition. It does not execute or
install a download. No public release feed is configured; a publisher signs
its own with `tools/sign_release.py`:

```sh
graphicle update --feed https://YOUR_RELEASE_HOST/stable.json --public-key YOUR_TRUSTED_PUBLIC_KEY
```

Live backend tests are marked and need the isolated services they name. The
model questions in the tests are development checks, not benchmarks.
[NOTICE.md](NOTICE.md) and [UPSTREAM.json](UPSTREAM.json) record the adapted
sources and their licences.
