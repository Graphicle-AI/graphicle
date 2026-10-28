# Graphicle with Muse Code

Muse Code is Meta's coding agent. `graphicle setup` connects it to
Graphicle's MCP server, which gives it the project's code graph,
whole-repository packs and the project's reviewed memory.

Graphicle's Muse Code plugin, a preview you add with one command, puts the
server, a skill and hooks in one package. The hooks are built to add
Graphicle's context without a tool call, at the start of each session and
after each code question, and to keep each session as a source of the
project, so that the next session can recall what this one decided.

**Status of the plugin.** It is built and tested against a stand-in `muse`
program that follows Muse Code's plugin contract. It has not yet run with
Muse Code on Meta's live API, so which hook events reach the model, and how
the context sits in each request, are as designed, not yet observed.
`bench/muse-code/hook_probe.py` checks the hook events in one short
session. Until that check has passed, setup does not install the plugin for
you.

## Connect

```sh
cd my-project
graphicle setup     # syncs the project and connects the agents it finds, Muse Code among them
```

Setup finds Muse Code by its settings folder, `~/.config/muse`, or the
`muse` program, and adds Graphicle's server to Muse Code's settings
([below](#the-mcp-server-alone)).

### Add the plugin (preview)

```sh
graphicle connect muse --hooks --allow-learning
muse plugins approve graphicle   # once: Muse Code reviews a plugin's hooks and servers
```

With the `muse` program on your `PATH` (or in `~/.local/bin`, where Muse
Code's installer puts it), `connect muse --hooks`:

1. writes the plugin into Graphicle's data folder (`muse-code-plugin/`),
   with the paths of your `graphicle` program and data folder;
2. backs up Muse Code's plugin records (`<file>.graphicle-bak`, once);
3. runs `muse plugins install <that folder> --scope user`, or
   `muse plugins update graphicle` when it was installed before;
4. removes the settings entry setup wrote for this project (the file backed
   up first), since the plugin's server replaces it;
5. starts building the project's session map in the background.

Muse Code runs the plugin's hooks and server only after you approve them:
until you run `muse plugins approve graphicle` (or approve it in
`/plugins`), Muse Code has no Graphicle server. Then start a new session.
`/mcp` lists the server and its tools; `muse plugins inspect graphicle`
shows the plugin.

- `--allow-learning` lets the agent propose memories, which a person
  reviews; without it the server publishes no proposal tools. `--tools` and
  `--work-state` reach the plugin's server as they reach a settings entry.
  `--name` does not apply: the plugin's id is always `graphicle`.
- Once the plugin is installed, setup and `graphicle connect muse` keep its
  paths current (after an upgrade) and add no settings entry beside it.
- Without the `muse` program, `connect muse --hooks` writes the settings
  entry instead, and says why.

To remove it, `graphicle disconnect muse` runs `muse plugins remove
graphicle` and deletes the package from the data folder. The plugin serves
every project, so a `graphicle disconnect` without a client, in one project,
leaves it; `graphicle disconnect --all` removes it too.

**Before you uninstall Graphicle or delete its data folder, run `graphicle
disconnect muse`.** The plugin's server sets `"mode": "optional"`, as the
settings entry does, and Muse Code accepts the field for a plugin's server,
but it does not show it, and it is not confirmed that Muse Code skips a
plugin's server that fails to start. If it does not, a plugin left behind
would stop every Muse Code session, in every folder, until you run `muse
plugins remove graphicle`. `bench/muse-code/hook_probe.py` records what Muse
Code does when a plugin's server cannot start, with and without the field.

## What the plugin holds

| Part | What it does |
|---|---|
| MCP server | `graphicle mcp --project . --or-workspace` with the flags you gave `connect`: the project of the folder Muse Code starts it in, else every project of the workspace, so it starts in any folder. The code graph's tools, cited search, `pack`, and with `--allow-learning` memory proposals that a person reviews. It sets `"mode": "optional"`, as the settings entry does; whether Muse Code then skips it when it cannot start is not confirmed ([above](#add-the-plugin-preview)). |
| Skill `graphicle` | When to call which tool: `pack` for a question across files, `query_graph` and `get_neighbors` for definitions, callers and callees, `recall_project` for past decisions, `propose_memory` to record one. |
| `SessionStart` hook | Adds the project's map (below) and its reviewed memories. |
| `UserPromptSubmit` hook | For a code question, adds the cited excerpts `graphicle context` finds, at most 8,400 characters, within the hook's time budget (10 seconds unless `prompt_context.seconds` says otherwise). |
| `PostToolBatch` hook | After a batch of tool calls that searched the files, adds one sentence on Graphicle's graph tools, at most five times a session. |
| `Stop`, `PreCompact`, `SessionEnd` hooks | Hand the session log to a detached `graphicle sessions import`: finished turns become sources of the project. |

The hooks never block Muse Code: each exits 0, the session start waits at
most 2.5 seconds for its context, and imports run in their own process.

The package itself, to install by hand, is in
[`packaging/muse-code-plugin/`](../packaging/muse-code-plugin/README.md).

### Context that keeps the prompt cache

The Meta Model API bills the longest start of a request it has seen recently
at its cached price ($0.15 instead of $1.25 per million input tokens on Muse
Spark 1.3, list prices on 26 October 2026). The plugin is designed to add its
context so that the start of every request in a session stays the same
bytes. This is the design; it has not yet been observed in Muse Code's
requests to the live API:

- **At session start:** the project's map comes first. It is the repository
  part of a cache-layout pack with `--prefix-share 1.0`
  ([packs](pack.md#layouts-and-the-prompt-cache)), 8,000 tokens by default:
  the architecture summary, a map of the folders, the most connected files
  and the top documentation. It is the same bytes for every session at a
  commit, so sessions can share it too. The reviewed memories follow it.
- **After a prompt:** the question's excerpts follow the prompt they answer,
  so the conversation before them stays as it was.
- **Never before every model call:** context added on `PreLLMCall` would
  change what each request starts with.

The map is kept in the data folder (`muse-code-packs/`) and rebuilt in the
background when it is older than ten minutes, so a session start never
waits for a pack; the first session after `connect muse --hooks` may start
without it.
`--pack-tokens N` after `hook muse session-start` in the plugin's manifest
sizes it (0 leaves it out, at most 100,000).

## Session memory

The plugin's hooks capture Muse Code sessions the way Claude Code and Codex
sessions are captured ([agents](agents.md#session-capture)):

- **Where Muse Code keeps them:** each session writes one log,
  `~/.local/share/muse/sessions/YYYY/MM/DD/<session id>/session.jsonl`
  (under `$XDG_DATA_HOME` when it is set). Sub-agents write their own logs
  under `subagent/`, which are not captured.
- **What becomes a source:** each finished turn: the person's prompt, the
  answer, the tool calls and their outputs. `<private>` spans and injected
  `<system-reminder>` blocks are dropped, credentials redacted, and tool text
  over 16 KiB replaced by its size and digest. Reasoning is never stored.
- **Import by hand:** `graphicle sessions import --client muse` reads every
  log under the session root; `--path` names one log or a folder of them.
- **Memories:** when the agent calls `propose_memory`, the proposal waits
  for review in the app (`graphicle open`). A memory you approve is in the
  next session's start, by title and id, and `recall_project` finds it with
  its sources.

Muse Code also keeps its own memory (`MEMORY.md` files). Graphicle's
memories are separate: approved by a person, cited to their sources, and
shared by every agent connected to the project.

`bench/muse-code/memory_demo.py` is a two-session example on a public
repository: the first session records a decision, you approve it, and the
second is asked about it and should cite the memory and the code. Its
Graphicle side (a draft is never in a session start, an approved memory is)
is tested; the example itself has not yet run with Muse Code on Meta's live
API.

## Contributor mode

Muse Code turns Contributor mode on by default for subscribers, which lets
Meta train on the content of your requests. With the plugin, those requests
hold your repository's code and memory: turn Contributor mode off in
`/settings` for private code, or run with `--contributor-mode off`.

## The MCP server alone

Without the plugin, `graphicle connect muse` adds one entry per project to
the `mcp_servers` object of `~/.config/muse/settings.json`:

```json
{
  "schema_version": 1,
  "mcp_servers": {
    "graphicle-my-project": {
      "transport": "stdio",
      "command": "/path/to/graphicle",
      "args": ["mcp", "--project", "<project id>"],
      "mode": "optional"
    }
  }
}
```

- Graphicle changes only its own entry; the file's other settings and
  servers stay. Muse Code requires `"schema_version": 1`, and Graphicle adds
  it when the file has none.
- `"mode": "optional"` lets a Muse Code session start, with a warning, when
  Graphicle is not installed.
- `graphicle disconnect muse` removes the entry.

## Use `pack` from Muse Code

The server gives Muse Code the code graph's tools (`query_graph`,
`get_node`, `get_neighbors` and more), cited search (`recall_project`,
`read_project_source`) and `pack`. The `pack` tool returns the project's code
for a question within a token budget (default 32,000 tokens, at most
200,000): an architecture summary, the most relevant code with line
numbers, callers and callees, tests, docs and a manifest of what is in and
what was left out ([packs](pack.md)).

```text
Call graphicle's pack tool for "how does the job controller count pod creations",
with a budget of 100000, then answer from it with path:line citations.
```

Muse Code's models, Muse Spark and Muse Wonder, have context windows of
1,048,576 tokens, so a large pack fits. For a pack larger than the tool's
200,000 tokens, use the command line: `graphicle pack "question" --budget
1000000 --out pack.md`, or `graphicle ask --pack` with the `meta` profile
([configuration](configuration.md#muse-spark)).

## Headless runs

`muse exec` runs one task without the interface, with the plugin's hooks
and server. Muse Code takes the Model API key from `META_API_KEY` before a
stored key or a browser sign-in, and API-key runs are billed by usage to
that key's account:

```sh
META_API_KEY=... muse exec --json --contributor-mode off "How does the scheduler pick a node?"
```

`--json` prints events as JSON lines, with a `run.usage` line before the run
ends. `bench/muse-code/` measures Muse Code with and without the plugin this
way.

## Claude Code and Codex on Meta's models

Meta documents running Claude Code and Codex on its models
(https://dev.meta.ai/docs/coding-agents.md). `graphicle setup` connects
Graphicle to them as to any other agent; only where they send their
requests changes. See [agents](agents.md#claude-code-and-codex-on-metas-models).
