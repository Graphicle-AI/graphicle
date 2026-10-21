# MCP servers

Graphicle's MCP server, `graphicle mcp`, serves one project's sources,
documents, reviewed memory and code graph in one connection. Its graph tools
keep Graphify's names (`query_graph`, `get_node`, `shortest_path` and the
others), so rules written for Graphify still apply. `graphicle graph serve`
stays as the graph-only server that matches Graphify, for a `graph.json` file.

| | Graphicle server | Graph-only server |
|---|---|---|
| Command | `graphicle mcp` | `graphicle graph serve` |
| Reads | The store in your data folder: sources, documents, reviewed memory, captured agent sessions and the project's recorded code graph | A `graph.json` file, by default `.graphicle/graph.json` |
| Best for | Code, documents and decisions together, with citations; how the code fits together | How the code fits together, matching Graphify |
| Transport | stdio; HTTP through `graphicle serve` | stdio; HTTP with `--transport http` |
| Set up with | `graphicle setup`, or `graphicle connect <client>` | The client's MCP settings (below) |
| Changes memory | Drafts for a person to review with `--allow-learning`, which `graphicle setup` and `connect --graph` turn on for every client but Claude Code; the agent's own to-do lists with `--work-state` | Never |

Neither server calls a model. The agent's own model reads what they return.

## Graphicle server

### Connect a client

`graphicle setup` connects every client it finds on your machine, and sets
each one up for the code graph as `connect --graph` does (below): Claude
Code gets the five essential tools (`--tools essentials`, below), and the
other clients the whole server, where the agent may propose memory drafts,
which a person reviews. `graphicle connect` without a client does the same.
[agents.md](agents.md) lists, for each client, what `setup` and `connect`
write, where, and how to undo it with `graphicle disconnect`.

To connect one client yourself:

```sh
cd ~/code/my-project
graphicle sync .                  # makes this folder a project
graphicle connect claude-code     # adds the server to .mcp.json in this folder
```

Restart or reload the client afterwards. `connect` connects the project of
the folder you run it in (outside every synced folder, the folder you synced
last). For Claude Code and VS Code, the entry in the folder finds the project
from the folder the client starts in, so the file holds no project id. Claude
Desktop, Cursor and Codex keep one settings file per user, so each project
gets its own entry, named `graphicle-<project name>`, that names the project.

| Client | Command | What it changes |
|---|---|---|
| Claude Code | `graphicle connect claude-code` | `.mcp.json` in the current folder |
| Cursor | `graphicle connect cursor` | `~/.cursor/mcp.json` |
| VS Code | `graphicle connect vscode` | `.vscode/mcp.json` in the current folder |
| Codex | `graphicle connect codex` | Runs `codex mcp add graphicle-<project name> …` |
| Claude Desktop | `graphicle connect claude` | `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `~/.config/Claude/claude_desktop_config.json` on Linux |

`connect` adds one entry and leaves the rest of the file alone. It takes
these options:

- `--project` connects another project: its id, its key or one of its folders.
- `--name NAME` names the entry.
- `--config-path PATH` writes to another settings file of the same format, with
  the project's id in the entry. It does not apply to Codex.
- `--replace` replaces an existing entry of the same name that differs from
  the new one. Without it, `connect` stops and leaves the entry alone.
- `--hooks` (claude-code and codex only) adds session capture. After each
  answer, a background process stores the session's completed turns as sources
  of the project. At the start of each session, the client receives one line
  on when to use Graphicle's tools and when grep is enough, then the titles
  of your reviewed memories and the latest turns (at most 8,000 characters,
  never drafts). The hooks never block the client. Claude Code's hooks go into
  `.claude/settings.local.json` in the current folder, and Codex's into
  `~/.codex/hooks.json`. Codex asks you to trust them first.
- `--work-state` adds `work_state_write` and `work_state_read`: to-do lists the
  agent keeps for itself. Open items appear at session start, labelled as not
  reviewed, and `graphicle work-state` shows them.
- `--allow-learning` adds the tools that propose memory drafts (below).
- `--tools SET` chooses the tools the entry publishes (as `graphicle mcp
  --tools`, below). Without it, Claude Code gets `essentials` and the other
  clients `all`.
- `--graph` sets the agent up for the code graph as well. For an entry of the
  whole server it implies `--allow-learning`.
- `--guards` (Claude Code) adds two guard hooks to
  `.claude/settings.local.json` (with no session capture unless you add
  `--hooks`). Before the agent searches (Grep, or a shell command such as
  `grep`, `rg` or `find`) in a folder whose project's code graph holds code,
  or reads a file of that project the graph has symbols in (or globs its
  source files), the hook points it to `query_graph`, `get_node`,
  `get_neighbors` and `recall_project`. In other folders, including a folder
  of conversation notes whose graph holds no code, the hooks answer as
  `graphicle graph install`'s guard does. Other clients get the entry only,
  because their hooks cannot add context before a tool call. If
  `graphicle graph install` also installed its guard hooks in the project,
  remove them with `graphicle graph uninstall`, or the agent gets two
  reminders.

A plain `graphicle connect <client>` after setup changes nothing: an entry
that setup wrote keeps `--allow-learning`, and without `--tools` an entry of
the same store and project keeps its tool set (Claude Code's whole-server
entry from 0.1.0's setup or `setup --tools all`, for example). Setup, and `connect` without a
client, run without `--guards`, remove the guard hooks an earlier setup
installed.

`graphicle disconnect <client>` removes what `connect` and setup wrote for
that client: its entry (the one `--name` named, or in `--config-path`'s file,
from the record Graphicle keeps in its data folder), and Graphicle's capture
and guard hooks. Every other entry, hook and setting stays as it was. A file
that is then the same as the backup Graphicle made before it first changed
it gets the backup back; a file `connect` created is removed once nothing
else is left in it. An entry of the
same name that does not run `graphicle mcp` is left alone. Codex's hooks file
(`~/.codex/hooks.json`) serves every project connected to Codex, so its
Graphicle hooks go with the last Codex connection. `graphicle disconnect`
without a client removes everything Graphicle added for this folder's
project, and `--all` everything for every project ([agents.md](agents.md)).

### Other clients

Any client that starts an MCP server over stdio can use the Graphicle server:

```json
{
  "mcpServers": {
    "graphicle": {
      "command": "/full/path/to/graphicle",
      "args": ["mcp", "--project", "PROJECT_ID"]
    }
  }
}
```

- `command -v graphicle` prints the full path. Use it, because a desktop
  client may not see your shell's `PATH`.
- The project ID is the `repository_id` that `graphicle sync` printed. It is
  also `repository.id` in the output of `graphicle status`.
- If you use a data folder other than the default, put
  `"--data-dir", "/path/to/data"` before `"mcp"`.
- `--allow-learning` adds the tools that propose memory, and `--work-state`
  adds the work-state tools.
- `--tools essentials` serves five tools: `query_graph`, `get_node`,
  `get_neighbors`, `recall_project` and `read_project_source`, with short
  instructions on when to call them and when grep is enough. Their
  descriptions start with what an agent looks for and say how answers cite
  code (file, lines and `stable_id`). `--allow-learning`, `--work-state` and
  `--pull-requests` add `propose_memory`, the work-state tools and the
  pull-request tools. This is what setup gives Claude Code, which shows the
  agent every tool's name and the instructions in each request.
- `--tools graph` serves every tool that reads the code graph: the graph
  tools and resources and the graph-history tools (`graph_episodes`,
  `graph_as_of`, `graph_history_stats`, `graph_link`), and on a workspace
  connection `workspace_map`. `--tools memory`
  serves the other tools. The default, `--tools all`, serves both. Two servers over one store,
  one of each, compare the integrated server with separate tools; the memory
  server's `recall_project` still reads the code graph unless it also has
  `--set graph_retrieval=false`.
- `--set graph_retrieval=false` (or `memory_retrieval=false`, or
  `doc_links`, `symbol_links`, `cross_repo`, `notes_first` or
  `identifier_route`, `true` or `false`, or `scope=project` or
  `scope=workspace`) fixes a switch of
  `recall_project` for the whole session (below); `--set doc_links=true`
  makes every recall follow documents' links to code, and
  `--set symbol_links=true` reviewed memory's links to code symbols. Repeat
  `--set` for several; a switch given twice, or one the server does not
  know, is refused. `--set scope=project` serves one project, so it is
  refused with `--workspace`, and `--set scope=workspace` needs
  `--workspace`.
- `--manifest` prints the server's manifest as JSON and exits: the
  program's version, edition and sha256, the tools and instructions it
  would serve with their sha256, the switches it fixes and those a call may
  set, the retrieval defaults, and the versions of the store's schema and
  of its export and receipt formats. Run it with the same options as the
  server to record exactly what a run used.
- `--pull-requests` adds the graph's pull-request tools, `list_prs`,
  `get_pr_impact` and `triage_prs`. They run the GitHub CLI and `git`,
  read-only, in the folder the server starts in, which must be the project's
  own folder, so the option works only over stdio there. `list_prs` reads no
  graph; the other two read the project's.

### Over HTTP

While `graphicle start` or `graphicle serve` runs, the same tools answer
JSON-RPC requests at `POST http://127.0.0.1:8854/mcp/WORKSPACE_ID/PROJECT_ID`.
Send the access key in an `Authorization: Bearer …` or
`X-Workspace-Capability` header, never in the URL. The app's **Agents & API**
page shows the endpoint and checks the connection. Give each agent its own
access key from **Workspace access**, not the owner's key. Before each tool
call, the endpoint syncs a few changed files of a project synced from a
folder, as the stdio server does
([configuration.md](configuration.md#freshness)).
`serve --mcp-learning` and `serve --mcp-work-state` publish the optional
tools, and `serve --mcp-tools` and `serve --mcp-set` work as `--tools` and
`--set` do. The pull-request tools are not served over HTTP, and a tool's
`repo` names another project by its id or key there, never by a folder.
The answer to `initialize` carries an `Mcp-Session-Id` header: a client that
sends it back with its later requests has them counted as one agent session
in the usage report, and a client that does not is served as before.

### Several projects in one connection

A workspace can hold several projects, for example a library and the
application that imports it. A connection to one project (`graphicle mcp`, or
`/mcp/WORKSPACE_ID/PROJECT_ID`) keeps recall, source reads and memory on that
project: its `recall_project` and `read_project_source` take no `repo`, and it
has no `list_projects` or `workspace_map`. Its graph tools and graph-history
tools (`query_graph`, `get_node`, `get_neighbors`, `shortest_path`,
`graph_as_of` and the others) do take `repo`: they read the code graph of
another project the access key can read, and over stdio `repo` can also name
a synced folder. Memory and documents stay on the connection's project there
too: `get_node` lists the reviewed memories linked to a symbol only for this
project's own code graph, and only this project's documents that name it.
`graphicle mcp` runs with the key saved in your data folder, normally the
owner's, which can read every project of the store.

To keep an agent to one project, code graph included, connect it over HTTP
with its own access key ([Over HTTP](#over-http)) and revoke that key's
access to each other project with the `graph.access` operation, run with the
owner's key:

```sh
graphicle call workspace.members        # lists each key's id
echo '{"credential_id": "KEY_ID", "state": "revoked"}' > revoke.json
graphicle call graph.access --project OTHER_PROJECT --arguments revoke.json
```

A key whose access to a project is revoked cannot read that project on any
connection: a workspace search leaves it out, and its source reads and graph
tools are refused.

To let an agent reach the other projects for recall too, give it a
**workspace connection** instead: `graphicle mcp --workspace` over stdio, or
`POST http://127.0.0.1:8854/mcp/WORKSPACE_ID` over HTTP. Do so only where
everything the key can read may reach the agent.

A workspace connection has no project of its own:

- `list_projects` lists the projects your key can read, with their ids, 100
  at a time (`offset` and `limit` page through them).
- `workspace_map` maps those projects ([The workspace
  map](#the-workspace-map)).
- `recall_project` searches every project you can read (at most 8, in the
  order they were created) unless `repo` names one: a project id or key, or
  with `graphicle mcp --workspace` a synced folder. A search over several
  projects holds each project's sources in memory until their results are
  interleaved, so it can use as much memory as that many single-project
  searches at once (up to 32 with `max_projects`, which `context` takes over
  HTTP and the SDK).
- `read_project_source`, the graph tools and the graph-history tools need
  `repo`.

It publishes only those tools and `graph_link`, and no resources (they
describe one project's graph). With `--workspace`, `graphicle mcp` builds the
waiting code graphs of the projects a search reads (the first 8) when it
starts; `--work-state` and `--pull-requests` need a project connection. A
workspace connection reads each project as of its last sync: the check for
changed files before each tool call applies to project connections, and the
Git hooks setup adds keep each set-up project current.

A search over several projects reads each project as a search of that project
alone would, under the same access key, and then interleaves their results:
each project's best excerpt, then each one's second, and so on, within one
token budget. Every excerpt names its project (`repository_id` and `project`
in the packet, and a `Project:` line in its text). The packet's `workspace`
field lists how many projects were searched and how many the limit of 8 left
out, and `projects` gives each project's own code-graph state and coverage.
A project whose search fails (a damaged store, for example) is left out with
`status: "error"` and its message, and the others still answer.

When `graphicle sync` (or `graph_link`) has recorded the imports between the
projects, the search follows them one step in each direction. An excerpt that
calls a function imported from another project brings that function's
definition along, and an excerpt that holds a function another project imports
brings along the code that uses it. These excerpts carry `cross_repo`: the
import, both ends and their stable ids. Turning the code graph off for a call
(`graph_retrieval: false`) turns this step off too.

A project your key may not read takes no part: it is not searched, no import
into it or out of it is followed, and it is not counted. Projects that
`graphicle pr` keeps for pull requests are not searched or listed either.
`graphicle context --workspace` runs the same search from the command line,
starting with the current folder's project. Answers (`graphicle ask`, the
`basic` and `reflect` jobs) still read one project.

#### The workspace map

`workspace_map` (the `graph.map` operation over HTTP, the SDK and
`graphicle call`) shows the projects your key can read, 8 at a time
(`offset` and `limit`, up to 32, page through them; a page reads its
projects' documents, as a workspace recall does), in the order they were
created:

- `projects`: each project's id, name and times; `graph`, the file set its
  code symbols were built from and whether that is the current one; and
  `counts`:
  - `files`, `documents` (connector documents your key can read now),
    `symbols` (code symbols in its code graph) and `memories` (as
    `project_memory` lists them);
  - `links`: `imports_out` and `imports_in` (recorded imports from and into
    the other projects on the map), `document_mentions` (mentions in its
    documents linked to code, as the `graph.doc_links` operation counts them)
    and `memory_symbols` (code symbols its memories are anchored to).
- `imports`: one entry for each pair of projects where a project on this
  page imports from another project on the map, with the number of recorded
  imports and the modules imported (the first 20, and `modules_total`).
  Paging through the map lists each pair once; `import_edges` counts every
  import between the projects on the map.

Each project is counted exactly as your key's own reads of that project
would count it, and a project your key may not read is not on the map: no
entry, no import into it or out of it, and no count that includes it.
Projects that `graphicle pr` keeps for pull requests are left out unless
`include_scratch` is true. A project whose counts cannot be read is listed
with `status: "error"` and its message. The map is read-only, and the imports
are the ones `graphicle sync` or `graph_link` last recorded.

### Tools

| Tool | What it does |
|---|---|
| `recall_project` | Finds original source evidence and current reviewed memory for a question, within a token budget; an identifier, or a question that only asks where one is defined or used, gets its definitions and references from the code graph |
| `read_project_source` | Reads a source by ID or path, continuing exactly where an excerpt stopped |
| `list_projects` | Lists the workspace's projects you can read, with the ids `repo` takes (workspace connections only) |
| `workspace_map` | Maps the projects you can read: their counts of files, documents, code symbols, memories and links, and the imports between them (workspace connections only) |
| `repository_status` | Shows source coverage, revisions and memory state |
| `repository_files` | Lists source paths and IDs |
| `project_memory` | Lists memories and drafts with their review state and freshness |
| `search_project_memory` | Searches reviewed memory by name, alias, scope and validity |
| `project_memory_history` | Shows a memory's revisions, optionally as known at a given time |
| `prepare_memory_refresh` | Prepares current evidence for updating a memory |
| `workflow_status` | Shows queued jobs and their model usage |
| `graph_episodes`, `graph_as_of`, `graph_history_stats` | Read the project's recorded code-graph history and its storage |
| `graph_link` | Links imports across the workspace's repositories, then recomputes the links from documents to the code they name. Only the projects that changed, or that import from a project whose exports changed, are linked again (`relinked`); the others keep their links (`reused`) |
| `prepare_observations` | Prepares source excerpts so the agent's own model can propose facts |
| `session_timeline` | Browses captured Claude Code and Codex turns |
| `query_graph` | Searches the code graph breadth-first or depth-first around a question and returns the relevant nodes and edges |
| `get_node` | Returns a node's details |
| `get_neighbors` | Returns a node's direct neighbours, with edge details |
| `get_community` | Returns the nodes of a community |
| `god_nodes` | Returns the most connected nodes |
| `graph_stats` | Returns node, edge and community counts and the confidence breakdown |
| `shortest_path` | Finds the shortest path between two nodes |

The server's instructions say when to call `recall_project` and
`query_graph` (where or how something works, questions across files or
history, before changing unfamiliar code) and that grep is enough for an
exact identifier the agent already knows. The descriptions of
`recall_project`, `query_graph`, `get_node`, `get_neighbors` and
`shortest_path` end with when to use each instead of grep.

The graph tools answer with the same text as the graph-only server, read on
the project's recorded code graph. They take two more arguments: `repo` reads
another project of the workspace (its id or key, or with `graphicle mcp` a
synced folder), and `as_of` reads an earlier graph (an episode id, a label
such as a commit sha, `latest` or `working`). They do not take
`project_path`. Their `structuredContent` (with `--tools essentials`, only
for a call that passes `structured: true`, an argument the tool schema does
not list) adds the episode read and the nodes
the answer names (up to 100), each with its `stable_id`, `revision` and line
range, and, when the file's current bytes are the ones the graph recorded,
its checked `file:` byte range and the file's sha256: the same `stable_id`
and window that `recall_project` cites for that symbol, which
`read_project_source` reads. A node whose file changed since is listed with
`checked: false`. The node of a `get_node` answer also lists, under
`linked`, the current reviewed memories anchored to that symbol (`memories`:
id, kind, title and how the link was made) and the documents you may read
that name it (`documents`); a stale, draft or retired memory is never listed.
Its citations block names them too. On a connection to one project, they are
that project's only ([Several projects in one
connection](#several-projects-in-one-connection)).
While a newer sync waits for its graph, the graph tools read the previous one
and say so (`episode_lag`); a node the answer reaches only through relations
read from changed files is listed with `relation_checked: false`, because
those relations may no longer hold. Because many clients show the model only
a tool's text, each graph tool's text is followed by a short citations block:
the graph read, how many files changed since when it is not the current one,
and for up to 20 of the nodes the answer names their `file:` line ranges, the
start of their `stable_id` and what is not checked. Communities are computed
when the server first reads a graph, so `get_community`, the community numbers and the
resources below are available. The project's graph is built with the
service's own options (receiver-aware calls, test identities, parallel edges
kept), so an answer can differ from `graphicle graph query` on a `graph.json`
of the same commit.

The resources `graphicle-graph://report`, `stats`, `god-nodes`, `surprises`,
`audit` and `questions` return the project graph's report and summaries, as
the graph-only server does for its file. The graph-only server's
`retrieve_context` is not served; use `recall_project`. If
`GRAPHICLE_QUERY_LOG` (or the other `GRAPHICLE_QUERY_LOG_*` variables) is set
in the server's environment, `query_graph` questions are logged as the
graph-only server logs them; nothing else is written.

With `--allow-learning`, three more tools are added: `propose_memory`,
`propose_observations` and `propose_memory_refresh`. They save drafts only.
Every quote in a draft must appear word for word in a current source, and a
person reviews each draft (in the app, under **Memory**) before recall uses
it. A quote that lies inside one code symbol anchors the memory to that
symbol: the reviewed memory goes stale when the symbol changes, and edits
elsewhere in the file usually leave it current
([the exact rule](../crates/graphicle/README.md#memory-anchored-to-code-symbols)).
`propose_memory` also takes `symbols` (a
`stable_id` from the code graph, or a `path` and `qualified_name`) and `files`,
which are whole-file dependencies; a path that is not in the project is
refused. With `--work-state`, `work_state_write` and `work_state_read` are added
to project connections; a workspace connection has no project to keep them
in.

With `--tools essentials` (what setup gives Claude Code), `recall_project`
answers with the packet's Markdown, which cites every excerpt, in at most
12,000 characters (room is kept for the line that lists the `needs` its
excerpts do not cover), and the graph tools answer with their text and the
citations block; neither sends a `structuredContent` copy. A call that
passes `structured: true` (not in the schemas) gets the whole packet or
answer in `structuredContent`, as 0.1.0 sent it; the Python SDK's
`Tools.call` asks for it. Every other set (`--tools all`, the default for the
other clients and for `graphicle serve`, and `graph` and `memory`) answers
as 0.1.0 did: the whole answer in `structuredContent` and again as JSON text,
recall at the budget asked for and in 0.1.0's order (test files are not
moved); `structured` changes nothing there.

`recall_project` uses the same retrieval defaults as `graphicle context`
([configuration.md](configuration.md#retrieval)). Its `skip_shown` option
replaces excerpts that the same connection has already returned with one-line
references, and `include` limits the search to sources whose id starts with
one of its prefixes (`file:` for the project's files, `file:docs/` for one
folder, `connector:<id>:` for one connector's documents). A harness can also
pass `graph_retrieval` or `memory_retrieval`, which the tool schema does not
list, as `true` or `false` for one call. `false` turns the code graph or
reviewed memory off. `true` does what leaving the switch out does, with one
addition: `graph_retrieval: true` also ranks the project's property graph of
sources and reviewed knowledge when no current reviewed memory exists (left
out, that graph is built only beside one). `doc_links`, also unlisted, is
about documents' links to the code they name (a Markdown code span, or a
connector's inline code, that names one definition). Left out, recall counts
the links of the excerpts it returns (the packet's `doc_links`, status
`counted`) and follows none. `true` follows them one step, for repository and
connector documents: an excerpt of a document brings in the definitions it
names, and an excerpt of code brings in the passages that name its
definitions. `false` reads no link and keeps documents and their links to
code out of the code graph and the property graph. `symbol_links`, also
unlisted, is about reviewed memory's links to the code symbols it is anchored
to. Left out, recall counts the links it would follow (the packet's
`symbol_links`, status `counted`) and follows none. `true` follows them one
step: an excerpt of code brings in the current reviewed memories linked to a
symbol inside it (marked `via: link`, with the link under `link`), and a
reviewed memory brings in the current code of the symbols it is linked to.
`false` reads no such link. `cross_repo`, also unlisted, is about the step
of a workspace recall that follows imports between projects: `false` turns
that step alone off, and `true` turns it on even with
`graph_retrieval: false`; left out, it follows `graph_retrieval`.
`notes_first: true`, also unlisted and off by default, orders notes and
documents first for a question in prose, quotes one window per file and
dates each excerpt ([Configuration](configuration.md#retrieval)); `--tools
all`, `graph` and `memory` keep 0.1.0's packet whatever it says.
`identifier_route`, also unlisted and on in the essentials set, answers an
identifier, or a question that only asks where named identifiers are
defined or used, with their definitions and references from the code graph
as `path:line` lines ([Configuration](configuration.md#retrieval)); `false`
keeps the full retrieval, and `--tools all`, `graph` and `memory` take the
route only when a call asks for it. The packet's `route` says which path
ran. On a
workspace connection a harness can likewise pass `scope`, `project` or
`workspace`, to search one project or all of them; a project connection takes
`project` only. Every packet's `mechanisms` echoes the values in force under
`switches` (`doc_links` and `symbol_links` read `counted` when left out), and
a switch that is not `true` or `false`, or a field the tool does not list, is
refused.

`recall_project`'s switches can also be fixed for a whole server:
`graphicle mcp --set graph_retrieval=false` (or `memory_retrieval`,
`doc_links`, `symbol_links`, `cross_repo`, `notes_first`,
`identifier_route` or `scope`;
`serve --mcp-set …` over HTTP) applies the value to every call, refuses a
call that asks for the other value, and reports the fixed switches in the
packet's `mechanisms` under `server_fixed`. For example,
`graphicle mcp --set doc_links=true` follows documents' links to code in
every recall of that server.

`graphicle sync` builds the project's code graph before it returns (or in a
background process with `--background`). When a sync used `--no-graph`, or
another process synced, `graphicle mcp` builds the waiting graph when it
starts, beside the requests it serves; `graphicle start` and `graphicle serve`
build it too. Until a graph is recorded, the graph tools and the
graph-history tools have nothing to show, and while a newer sync waits for its
graph, recall reads the previous one and cites only the files that have not
changed since.

## Graph-only server

`graphicle graph serve` is the graph engine's own server, which matches
Graphify on a `graph.json` file. Build the graph first, then start the server
on it:

```sh
graphicle graph extract . --code-only
graphicle graph serve .graphicle/graph.json
```

To add the graph server to a client, give the client the command and the
graph's full path. With Claude Code, for example, `.mcp.json` would hold:

```json
{
  "mcpServers": {
    "graphicle-graph": {
      "command": "/full/path/to/graphicle",
      "args": ["graph", "serve", "/full/path/to/my-project/.graphicle/graph.json"]
    }
  }
}
```

Cursor uses the same format in `~/.cursor/mcp.json`. VS Code's `.vscode/mcp.json`
uses `"servers"` in place of `"mcpServers"` and needs `"type": "stdio"` in the
entry.

### Options

| Option | Effect |
|---|---|
| `--transport http` | Serves streamable HTTP instead of stdio, at `http://127.0.0.1:8080/mcp` |
| `--host HOST`, `--port N`, `--path /mcp` | Changes the address (defaults: 127.0.0.1, 8080, `/mcp`) |
| `--api-key KEY` | Requires the key in an `X-API-Key` or `Authorization: Bearer` header. `GRAPHICLE_MCP_API_KEY` sets it too. Use a key whenever the server listens beyond loopback |
| `--json-response`, `--stateless`, `--session-timeout S` | Change the HTTP session behaviour |
| `--watch [--root DIR]` | Keeps the graph in memory and applies each saved file at once, while the normal rebuild of `.graphicle/` runs in the background |
| `--retrieval` | Adds the `retrieve_context` tool: ranked, cited source evidence within a token budget |
| `--min-evidence LEVEL` | Leaves out call edges below an evidence level: `name`, `binding`, `type`, `indexer` or `runtime` |

### Tools and resources

| Tool | What it does |
|---|---|
| `query_graph` | Searches the graph breadth-first or depth-first and returns the relevant nodes and edges |
| `get_node` | Returns a node's details |
| `get_neighbors` | Returns a node's direct neighbours, with edge details |
| `get_community` | Returns the nodes of a community |
| `god_nodes` | Returns the most connected nodes |
| `graph_stats` | Returns node, edge and community counts and the confidence breakdown |
| `shortest_path` | Finds the shortest path between two nodes |
| `list_prs`, `get_pr_impact`, `triage_prs` | Show open GitHub pull requests with their impact on the graph. They need the GitHub CLI |

The resources `graphicle-graph://report`, `stats`, `god-nodes`, `surprises`,
`audit` and `questions` return the graph report and its summaries.

### Agent skills and hooks without MCP

Agents can also use the graph through its command line. These commands set
that up:

- `graphicle graph install` installs a skill that teaches Claude Code to run
  `graphicle graph query` and the related commands. Use `--platform NAME` for
  other agents, such as `codex`, `cursor`, `gemini`, `vscode`, `opencode`,
  `aider` and `kiro`; `graphicle graph --help` lists them all. Add `--project`
  to install into the current project instead of your home folder.
- `graphicle graph claude install` adds a section to the project's `CLAUDE.md`
  and a hook that reminds the agent to check the graph before it searches or
  reads files. `graphicle graph codex install`, `cursor install` and the other
  platforms write the equivalent rules for their agent.
- `graphicle graph hook install` adds Git hooks that update the graph after
  each commit and checkout.

`graphicle graph uninstall` removes the skills and rules. These commands and
`graphicle connect` write separate entries, so they never overwrite each
other's settings.
