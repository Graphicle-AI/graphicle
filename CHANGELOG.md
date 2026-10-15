# Changelog

Notable changes to Graphicle: the graph engine (`graphicle-graph`), the memory
library (`graphicle-memory`) and the `graphicle` program.

## 0.1.1 - unreleased

Every crate is at 0.1.1, and `graphicle --version` prints
`graphicle 0.1.1 (core)`.

### Code graph

- The engine follows Graphify 0.9.91 (0.1.0 followed 0.9.88): the three
  releases 0.9.89 to 0.9.91 are ported, and the comparison checks pass
  against 0.9.91. What changes in the graph:
  - Go: a package type whose methods span several files is one node, and
    the methods of a generic receiver (`func (b *Box[T])`) attach to `Box`.
    Member calls on a typed receiver are bound; when the receiver's type is
    declared in another repository (Go, PHP, TypeScript and JavaScript,
    Kotlin, Objective-C), the call is bound by `merge-graphs` and
    `global add`.
  - C#: `foreach` loop variables are typed; member calls bind only through a
    typed receiver; names that differ only by `_` or by case keep separate
    nodes; partial classes merge after colliding ids are separated; `new X()`
    no longer binds to a method named `X`.
  - Generic and template type parameters of Kotlin, Scala, C++ and Swift no
    longer create stub nodes. Objective-C `NS_ENUM` and `NS_OPTIONS` give an
    enum type with its members. An interface member links to every
    implementer.
  - JavaScript and TypeScript: a `.js` import resolves to a `.tsx` source
    when no `.js` file exists; a file-scope callback's parameters shadow
    same-named functions; a class field's type is scoped to its class.
    Python module calls (`mod.func()`) resolve inside methods and nested
    functions. A Rust `use crate::…::Name` edge lands on the declaration it
    imports. A symbol whose id spells another file's id keeps its own node.
  - Jinja templates (`.j2`, `.jinja`, `.jinja2`) are documents. An
    incremental build keeps an edge between two kept nodes even when its
    source file was not re-extracted, and sweeps orphaned placeholder nodes.
    Dedup keeps a document beside a same-named top-level file. A community
    keeps its curated or LLM label when its members mostly stay, and
    fallback labels go to `.graphicle_labels.pending.json`.
  - `hook install` also installs a `post-merge` hook; a read or search nudge
    names the graph nodes behind it; `pi guard install` adds a Pi guard
    extension; Ollama requests go to `/api/chat` (with a fallback for
    servers without it); colliding downloaded captures get numbered names.
  - The AST cache schema moves to 9, so the first build after upgrading
    re-extracts. The writer stamp in `graph.json` names 0.9.91.
- Go call binding reads each `go.mod` once per pass instead of once for
  every candidate of every Go call.

### How the engine holds its data

- During assembly, the per-file results, their raw calls and the edges are
  compact records, each value stored once in a table they share, instead of
  one JSON object each. The resolvers, the build, `graph.json`'s writer and
  the update path read them in place; the Java type passes borrow them
  instead of copying them.
- Community detection runs on the graph itself instead of on two copies of
  it; call binding and the member-call resolvers keep compact pair tables;
  the graph's indices and adjacency rows are compact; an update keeps its
  stored graph out of memory once it has read it; the per-file records are
  freed after the last pass that reads them, and freed pages go back to the
  system at stage boundaries.
- None of this changes the graph: on the repositories checked, `graph.json`
  is byte-identical with and without these changes. The graph changes under
  Code graph come from following Graphify 0.9.91.

### Agents and setup

Less fixed text in every agent request. In a pre-registered test of 0.1.0
(100 SWE-bench Verified tasks, Claude Code), the agents never called
Graphicle's tools, and setup's text was in every request. 0.1.1 makes no
token or accuracy claim; it trims that text.

- `graphicle setup` gives Claude Code five tools instead of 25:
  `query_graph`, `get_node`, `get_neighbors`, `recall_project` and
  `read_project_source` (`graphicle mcp --tools essentials`). Claude Code
  shows the names of a server's tools and its instructions in every request,
  and loads a tool only when the agent searches for it. The five come with
  one sentence of instructions on when to call them; their descriptions
  start with what an agent looks for and say how answers cite code.
  `setup --tools all` gives Claude Code every tool, as 0.1.0 did; the other
  clients keep every tool.
- The guard hooks that point Claude Code's searches to the graph tools are
  off unless asked for: `setup --guards` (or `connect --guards`). Running
  setup again without it removes the guard hooks an earlier setup installed.
- The Graphicle section of `AGENTS.md` and `CLAUDE.md` is two lines. Setup
  adds none when Claude Code is the only agent connected; a section an
  earlier setup wrote is kept and brought up to date. `setup --tools all`
  writes 0.1.0's section, whichever agents are connected.
- `graphicle graph benchmark` calls its figure what it is: the size of the
  whole corpus over the size of one graph answer, a size ratio, not what an
  agent saves. Its JSON keeps Graphify's field names.
- Claude Code's session-start hook prints nothing when the project has no
  reviewed memory, no open work and no stale memory.
- `graphicle connect claude-code` without `--tools` keeps an existing entry
  of the same store and project whatever its tool set, such as 0.1.0's
  setup's or `setup --tools all`'s, instead of refusing it. `doctor --fix`
  brings back an entry that names no `--tools`, or is gone, as the whole
  server.
- `setup --tools all --guards` restores 0.1.0's setup for Claude Code (every
  tool, 0.1.0's instructions and replies, the guard hooks, 0.1.0's
  `AGENTS.md` section) except for two things: the session-start hook still
  prints nothing when there is nothing reviewed, open or stale (0.1.0
  printed the last captured turns), and `graphicle context` in a terminal
  still puts test files last by default (`include_tests: true` through
  `graphicle call context` gives 0.1.0's order). A call that passes
  `structured` to recall or a graph tool, which 0.1.0 refused, is
  accepted and changes nothing.

### Context and recall

- `context`, and `recall_project` in the essentials set, present excerpts
  of test files after the other excerpts unless the question names a test
  file or a test identifier (`tests/...`, `test_x`, `x_test.py`,
  `conftest.py`, `TestX`). A test file is one in a `test`, `tests` or
  `testing` folder, or a `test_*`, `*_test.py`, `*_tests.py` or
  `conftest.py` file; other languages' test names (`x_test.go`,
  `x.spec.ts`) are not moved. Only the order changes: the budget goes to
  the excerpts in rank order first, so a packet holds the same excerpts as
  0.1.0's. The new `include_tests` argument of `context` (`graphicle call
  context --arguments`, the HTTP API) sets this per call: `"auto"` (the
  default), `false` (always after) or `true` (0.1.0's order), and the
  packet's `test_files` says what applied.
- With `--tools essentials`, `recall_project` answers with the packet's
  Markdown only, at most 12,000 characters (room is kept for the line that
  lists the `needs` the excerpts do not cover), and the graph tools with
  their text and citations, without a `structuredContent` copy.
  `structured: true` returns 0.1.0's reply; the Python SDK asks for it.
  Every other set (`--tools all`, which the other clients and `graphicle
  serve` get by default, `graph` and `memory`) answers exactly as 0.1.0
  did: the same reply, recall at the budget asked for and in 0.1.0's
  order.

### Security

- rustls 0.23.46, which fixes RUSTSEC-2026-0346, RUSTSEC-2026-0347,
  RUSTSEC-2026-0348 and RUSTSEC-2026-0349 (TLS, published on 14 October
  2026). Only the lock files change.

### Windows (preview)

- Release archives for Windows on x86-64,
  `graphicle-<version>-x86_64-pc-windows-msvc.zip`, with `graphicle.exe` and
  `graphicle-graph.exe`. The programs link the C runtime statically, so they
  run without the Visual C++ runtime; the release build checks that they
  import Windows system libraries only.
- `install.ps1` installs both programs from PowerShell (Windows PowerShell 5.1
  or PowerShell 7) into `%LOCALAPPDATA%\Programs\Graphicle\bin`, checks the
  archive's SHA-256 first, adds the folder to the user `PATH` and needs no
  administrator rights. Run again, it replaces programs that are running.
- Continuous integration builds and lints each crate on Windows and runs its
  tests.
- `docs/install.md` covers installing and building on Windows, what the
  preview leaves out, and troubleshooting. Without Python 3.14 or the
  `zstd` program, `prepare_parser_sources.py` on Windows suggests
  `py -m pip install zstandard`.
- The engine and the `graphicle` program on Windows: one path spelling with
  `/` in the paths the engine reports, path comparisons that ignore case and
  `\\?\` prefixes, network-share (UNC) roots, Graphify's Windows lock for
  rebuilds started by Git hooks, Ctrl+C and Ctrl+Break handling, and colour
  only where the console supports it.
- On Windows, `graphicle` keeps its data in `%LOCALAPPDATA%\Graphicle`, finds
  the settings of Claude Desktop (including its Microsoft Store build) and
  VS Code in their Windows folders, finds programs with the extensions in
  `PATHEXT`, and starts background work without a console window.
- On Windows, the memory library reads a source named in a graph only when
  its path stays inside the project folder: no drive, network-share, rooted
  or `..` paths.

### Other changes

- The repository's `.gitattributes` checks text files out with LF line
  endings on every system.

## 0.1.0 - 2026-10-14

The first public release. Every crate is at 0.1.0, and
`graphicle --version` prints `graphicle 0.1.0 (core)`.

### Code graph

- A Rust graph engine compatible with Graphify 0.9.88. Comparison checks run
  Graphify's own tests against the engine and compare its results with those
  of Graphify 0.9.88; `docs/benchmarks.md` gives the checks, their results
  and their limits. Where an upstream test fails against the engine,
  `docs/evidence/upstream-tests-known-differences.md` records it with the
  reason, and `docs/evidence/upstream-tests-not-run.md` lists the test files
  the checks do not run.
- `graphicle graph <command>` runs the engine (`graphicle graph update .`),
  and the engine also ships as its own program, `graphicle-graph`.
- Opt-in additions beyond Graphify: `--precise`, `extract --docs`,
  `extract --routes`, `retrieve` and `serve --watch`.

### Memory, recall and answers

- `graphicle sync` keeps a project's code, documents and captured agent
  sessions in one local store that keeps the original sources, and builds the
  project's code graph.
- `graphicle context` recalls cited excerpts for a question from the sources,
  the code graph and reviewed memory, within a token budget.
- `graphicle ask` answers with numbered citations, using a model you
  configure (`graphicle config set model anthropic`, `openai` or `ollama`;
  for Ollama it uses the server `OLLAMA_HOST` names, as `ollama` does, and
  says whether Ollama answers and has the model). Without a model it prints
  the cited sources and how to add one.
- Ollama requests turn thinking off (`think: false`), so a thinking model
  such as the default `qwen3:4b` writes its answer within its output limit.
- A citation whose quote differs from its source only in spaces, tabs or
  line breaks (a model that joins blank lines, re-indents code or writes a
  line break as `\n`) is kept as the source's exact text, in `ask`,
  `--workflow reflect` and its review; an answer that abstains drops the
  citations it cannot support. Quotes that differ in anything else still
  fail the answer.
- When a provider refuses a request, the error gives the provider's own
  reason (never the key), and `ask` says what to change. `--workflow reflect`
  says when its review call failed and the answer was not reviewed, and
  shows the file and lines of each citation.
- With `structured_output`, the Anthropic transport offers its result tool
  without forcing it, and the OpenAI-style protocols leave `minLength` and
  `maxLength` out of the schema they send; every reply is still checked
  against the whole schema.
- Agents can propose memories, which wait as drafts until a person reviews
  them.
- A memory whose quote lies inside one function, class or method is anchored
  to that code symbol, by its `stable_id` and revision. Reviewed memories,
  retained answers and member notes then go stale when the symbol changes or
  is removed (`symbol_changed`, `symbol_missing`): an edit to the symbol's
  lines, from its start up to the next definition (or the end of the file),
  withholds the memory, and other edits to the file do not. A file moved
  with its content unchanged is followed, and while a sync waits for its
  code graph a memory on a changed file is held back (`projection_pending`).
  `propose_memory` and `note` also take `symbols` (a `stable_id`, or a path
  and qualified name) and whole-file `files`, and the app's memory form has a
  symbol picker.
- Recall across the projects of a workspace: `graphicle context
  --workspace`, `scope: "workspace"` over HTTP and the SDK, and a workspace
  MCP connection search every project the access key may read (at most 8 by
  default, `max_projects` up to 32). Each project is read and authorized as a
  search of that project alone would be, the results are interleaved within
  one token budget, every excerpt names its project, and the imports between
  the projects that `sync` recorded are followed one step each way. Answers
  (`ask`) still read one project.
- Documents are linked to the code they name: a Markdown code span, a
  reStructuredText role or literal, and the inline code of Jira, Notion,
  Confluence, GitHub and Slack text link to a code symbol when they name
  exactly one definition, in the document's own project first, then in the
  other projects of its workspace; plain words and ambiguous names never
  link. `graphicle call graph.doc_links` lists the links, counting only what
  the caller may read. Recall counts the links of the excerpts it returns
  without following them; `doc_links: true` (or `graphicle mcp --set
  doc_links=true`) follows them one step each way, and `doc_links: false`
  reads none and keeps documents out of the code-graph part of the ranking.

### Setup and agents

- `graphicle setup` sets up a repository in one command: it syncs the folder,
  builds the code graph, connects the coding agents it finds (Claude Code,
  Codex, Cursor, VS Code and Claude Desktop), turns on session capture, adds
  a marked section to `AGENTS.md` and marked lines to the Git hooks. It lists
  and backs up every file it changes (the backups, and a
  `.claude/settings.local.json` it creates, stay out of Git), and is safe to
  run again.
- `graphicle connect`, `graphicle disconnect`, `graphicle uninstall` and
  `graphicle doctor` (with `--fix`) connect agents, undo what Graphicle added,
  and check and repair the setup. `graphicle uninstall` says how to remove the
  program the way it was installed (Homebrew, Docker, Cargo, the install
  script or a source build).
- In an agent's settings file with comments (VS Code's JSONC), Graphicle's
  entry goes on lines of its own, every comment stays where it was, and
  `graphicle disconnect` keeps comments you changed while the agent was
  connected.
- Each agent that `graphicle setup` (or `graphicle connect` without an
  agent) connects gets one MCP server with the code-graph, recall and memory
  tools, `graphicle mcp --allow-learning`: the agent may propose memory
  drafts, which a person approves before recall uses them. Claude Code also
  gets two guard hooks that point its searches and file reads in a project
  with code to the graph tools. `connect <agent> --graph` sets up one agent
  the same way, and `disconnect` removes the hooks too.
- The guard hooks nudge each agent at most five times a session, the first
  time always; `GRAPHICLE_HOOK_NUDGE_CAP` sets another limit, and `0` lifts it.

### MCP

- `graphicle mcp` is one MCP server for any MCP client: `recall_project`
  returns original source evidence and current reviewed memory,
  `--allow-learning` adds the tools that propose memory, and the graph
  engine's tools answer under Graphify's names (`query_graph`, `get_node`,
  `get_neighbors`, `get_community`, `god_nodes`, `graph_stats` and
  `shortest_path`), with its resources, from the code graph that
  `graphicle sync` records. Each graph answer is the engine's text, then a
  citations block that names up to 20 of its nodes by file, line range and
  `stable_id` (the one `recall_project` cites for that code);
  `structuredContent` cites up to 100, with the checked byte range of each
  node whose file has not changed since. `as_of` reads an earlier graph.
  `--tools graph|memory` serves one half of the tools, `--set` fixes a
  `recall_project` switch for the session, and `--pull-requests` adds the
  engine's pull-request tools, which run the GitHub CLI, over stdio in the
  project's folder. While `graphicle serve` runs, the same tools answer over
  HTTP (`serve --mcp-tools` and `--mcp-set` work as `--tools` and `--set`).
- A connection to one project keeps recall, source reads and memory on that
  project. Its graph tools and graph-history tools read another project's
  code graph when `repo` names one the access key can read (over stdio, also
  a synced folder), and `graphicle mcp` runs with the key saved in the data
  folder, normally the owner's, which reads every project. To keep an agent
  to one project, connect it over HTTP with its own key and revoke that
  key's access to the other projects with `graph.access` (`docs/mcp.md`). A
  workspace connection (`graphicle mcp --workspace`, or
  `POST /mcp/WORKSPACE_ID`) serves every project the key can read, with
  `list_projects`.

### Freshness

- Before `ask`, `context`, `status` and each agent tool call (through
  `graphicle mcp` or the HTTP MCP endpoint of `graphicle serve`), Graphicle
  checks whether files changed since the last sync. It syncs a few changed
  files first, and otherwise says how many changed and suggests
  `graphicle sync`. The Git hooks sync in the background after commits,
  merges and checkouts. `graphicle config set freshness off` turns the check
  off.
- A sync of a folder whose files did not change converts no document again,
  so the check stays quick on large folders.

### Web app and SDKs

- `graphicle open` opens the web app (ask, sources, memory and the code
  graph); `graphicle serve` runs it with the HTTP API, also as a shared
  server for a team that manages its own keys (`docs/team.md`).
- A Python SDK (`graphicle-sdk`, imported as `graphicle_sdk`) and a
  TypeScript SDK talk to a running server. Neither is on a package registry
  yet; install them from `sdk/`.

### Limits

- A project takes up to 100,000 text files and 1 GiB of text, and a text
  file up to 8 MiB. Whoever runs Graphicle can raise each limit with
  `limits.<name>` (`graphicle config set limits.project_files 250000`) or a
  `GRAPHICLE_MAX_<NAME>` environment variable; a request through the API or
  MCP cannot. A project over a limit is refused with a message that names
  the limit and its setting, and one file never stops a sync: files past the
  analysis limits are analysed as far as the limits allow, and files that
  cannot be read are listed with the reason. `graphicle config show` lists
  the limits in effect, and `graphicle config get limits.<name>` prints one.
  The "Limits" section of `docs/configuration.md` lists every limit.

### Privacy

- Local first: the store is one SQLite file in your data folder, and the app
  and its HTTP and MCP endpoints listen on 127.0.0.1 by default.
- No telemetry and no account. Graphicle connects to the network only when
  you ask it to, for example for a model you configure or a connector you
  add; the README's privacy section lists every case.
- Sync respects `.gitignore` and skips symlinks, `.env` files, keys and
  credential files; session capture redacts credentials before anything is
  stored.

### Install

- macOS and Linux (glibc), on arm64 and x86_64: the Homebrew tap
  (`brew install graphicle-ai/tap/graphicle`), the install script, which
  checks each archive's SHA-256, or the release archives. There is no
  native Windows build yet.
