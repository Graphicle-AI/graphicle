# Agents

Coding agents use Graphicle through MCP, the protocol they use to call tools.
`graphicle setup` connects the agents it finds on your machine to one MCP
server, `graphicle mcp`: the project's code graph (with Graphify's tool
names, such as `query_graph` and `get_neighbors`), search over the code,
documents and reviewed memory, and the memory tools. This page lists, for
each agent, what setup and `graphicle connect` write, where, and how to undo
it.

## Connect and disconnect

```sh
graphicle setup                  # set up this folder and connect the agents it finds
graphicle connect                # connect the agents it finds (it asks on a terminal)
graphicle connect --all          # the same, without asking
graphicle connect cursor         # connect one agent
graphicle connect cursor --graph # connect one agent as setup does (below)
graphicle disconnect cursor      # remove that agent's entry and hooks
graphicle disconnect             # remove everything Graphicle added for this folder's project
graphicle disconnect --all       # the same for every project
graphicle uninstall              # disconnect everywhere, then show how to remove the program
```

The agent names are `claude-code`, `codex`, `cursor`, `vscode` and `claude`
(Claude Desktop). `setup --clients` also takes the numbers setup shows on a
terminal. Restart or reload an agent after you connect or disconnect
it. `graphicle status` lists the connected agents and whether each entry still
works.

Setup, and `connect` without an agent, set each agent up for the code graph,
as `connect <agent> --graph` does:

- **Memory drafts.** The entry runs `graphicle mcp --allow-learning`, so the
  agent can propose a memory: a decision, a procedure or a note, quoting
  current sources word for word. A draft is only a draft. Recall and session
  start use it only after a person approves it in the app (**Memory**).
- **The graph guard (Claude Code).** Two hooks in
  `.claude/settings.local.json` run before Claude Code searches (Grep, or a
  shell command such as `grep`, `rg` or `find`) or reads a source file in a
  project whose code graph holds code. They point it to `query_graph`,
  `get_node`, `get_neighbors` and `recall_project` first; reading a file to
  change it stays the agent's choice. In other folders they pass the request
  to the graph engine's own guard. Each agent gets at most five of these
  nudges a session (a subagent counts on its own, and the first nudge always
  comes); the environment variable `GRAPHICLE_HOOK_NUDGE_CAP` sets another
  limit, and `0` lifts it. The other agents' hooks cannot add context before
  a tool call, so they get the entry only.

A plain `graphicle connect <agent>` writes the entry without `--allow-learning`
and adds no guard hooks; run after setup, it keeps setup's entry as it is.

## How Graphicle edits your files

- **Only its own entries.** Graphicle adds an MCP entry named `graphicle` or
  `graphicle-<project>`, hooks that run `graphicle hook`, and sections and
  lines between markers that name Graphicle. Everything else in each file
  stays as it was.
- **A backup first.** Before it first changes a file, Graphicle copies it to
  `<file>.graphicle-bak` beside it. Later runs keep that first copy. In a Git
  repository, a marked `*.graphicle-bak` line in `.git/info/exclude` keeps
  the backups out of `git status` (a copy of an ignored file, such as
  `.claude/settings.local.json`, stays ignored), and syncs never read them.
- **Safe to repeat.** Running setup or connect again changes nothing that is
  already in place, and says so.
- **No keys in files.** The model settings name the environment variable that
  holds a key, never the key itself.
- **Undo by name.** `graphicle disconnect` finds Graphicle's changes in the
  record it keeps in the data folder, and by their entry names and markers,
  and removes only those. When a file is then the same as its backup, the
  backup is put back, so the file is exactly as it was.

A project is a folder you set up, and `<project>` is its name in lower case,
for example `graphicle-my-app`. When another project already has that name
(two repositories called `api`), the entry gets the project's short id too,
for example `graphicle-api-1a2b3c4d`, and each keeps its own. Agents with a settings file per folder (Claude
Code and VS Code) get one entry named `graphicle` that finds the project from
the folder they start in. Agents with one settings file for your user account
(Codex, Cursor and Claude Desktop) get one entry per project.

## Claude Code

| What | Where | Written by |
|---|---|---|
| MCP server `graphicle` | `.mcp.json` in the project folder | setup, `connect claude-code` |
| Session capture hooks | `.claude/settings.local.json` in the project folder | setup (unless `--no-capture`), `connect claude-code --hooks` |
| Graph guard hooks | `.claude/settings.local.json` in the project folder | setup, `connect claude-code --graph` |
| Graphicle section | `CLAUDE.md`, if the project has one | setup (unless `--no-instructions`) |

- Claude Code asks you to approve the servers in a project's `.mcp.json`
  the first time it starts there.
- `.claude/settings.local.json` holds your personal settings, and the hooks
  name the program on your machine. Keep this file out of version control.
- If `graphicle graph install` (or `graphicle graph claude install`) also
  added the graph engine's guard hook in the project, remove it with
  `graphicle graph uninstall`, or Claude gets two reminders.
- Undo: `graphicle disconnect claude-code`. By hand, delete the `graphicle`
  entry from `.mcp.json` and the hooks whose command contains
  `hook claude-code` (capture and guard hooks) from
  `.claude/settings.local.json`.

## Codex

| What | Where | Written by |
|---|---|---|
| MCP server `graphicle-<project>` | Codex's own settings, through `codex mcp add`; without the Codex command line, `~/.codex/config.toml` (`$CODEX_HOME/config.toml`) between Graphicle's markers | setup, `connect codex` |
| Session capture hooks | `~/.codex/hooks.json` (`$CODEX_HOME/hooks.json` when that is set) | setup (unless `--no-capture`), `connect codex --hooks` |
| Graphicle section | `AGENTS.md` in the project folder | setup (unless `--no-instructions`) |

- With the Codex command line, `codex`, on your `PATH`, Graphicle adds the
  entry through it. Without it (the Codex IDE extension alone), Graphicle
  writes the entry's table into `config.toml` itself, between the lines
  `# graphicle-start (added by graphicle connect; graphicle disconnect removes it)`
  and `# graphicle-end`. A server of the same name that is not Graphicle's is
  left alone.
- Codex asks you to review and trust new hooks before they run.
- Undo: `graphicle disconnect codex`. By hand, run
  `codex mcp remove graphicle-<project>` (or delete the lines between
  Graphicle's markers in `config.toml`) and delete the hooks whose command
  contains `hook codex` from `hooks.json`.

## Cursor

| What | Where | Written by |
|---|---|---|
| MCP server `graphicle-<project>` | `~/.cursor/mcp.json` | setup, `connect cursor` |

- Undo: `graphicle disconnect cursor`, or delete the entry by hand.

## VS Code

| What | Where | Written by |
|---|---|---|
| MCP server `graphicle` | `.vscode/mcp.json` in the project folder | setup, `connect vscode` |

- The entry passes VS Code's workspace folder to Graphicle, which finds the
  project from it.
- Undo: `graphicle disconnect vscode`, or delete the `graphicle` entry under
  `servers` by hand.

## Claude Desktop

| What | Where | Written by |
|---|---|---|
| MCP server `graphicle-<project>` | `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `~/.config/Claude/claude_desktop_config.json` on Linux | setup, `connect claude` |

- Claude Desktop has no project folder, so each project you connect gets its
  own entry.
- Undo: `graphicle disconnect claude`, or delete the entry by hand.

## Other MCP clients

Any client that starts an MCP server over stdio, such as Windsurf, can use
Graphicle. [mcp.md](mcp.md#other-clients) shows the entry to add by hand.
`connect` also writes to another settings file in the same format with
`--config-path PATH`. [mcp.md](mcp.md#connect-a-client) lists every option of
`connect`.

## In the repository

### Agent instructions

Setup adds a short Graphicle section to `AGENTS.md` in the project folder, and
to `CLAUDE.md` if it exists. Many agents read these files. The section tells
them to use Graphicle's tools for questions about the code, earlier decisions
and earlier sessions.

- The section sits between the lines
  `<!-- graphicle:start (added by graphicle setup; graphicle disconnect removes it) -->`
  and `<!-- graphicle:end -->`, under the heading `## Graphicle`.
- If `AGENTS.md` does not exist, setup asks before it creates it. `--yes`
  answers yes.
- `--no-instructions` skips this step.
- Undo: `graphicle disconnect` removes the section. By hand, delete the
  lines between Graphicle's markers.

### Git hooks

Setup adds a few marked lines to three Git hooks: `post-commit`, `post-merge`
and `post-checkout`, in the hooks folder Git uses for the repository
(`.git/hooks`, or the folder Git's `core.hooksPath` setting names when it is
inside `.git`). The lines start a quiet `graphicle sync` of the project's
folder in the background, so Git does not wait, and your next question sees
the new code.

- Existing hooks are kept. Graphicle adds its lines before a final `exit` or
  `exec` line, else at the end of the file.
- The lines sit between `# graphicle-sync-start <folder>` and
  `# graphicle-sync-end`.
- A hooks folder outside `.git` (one kept in the repository, such as Husky's,
  or one shared by all your repositories) is left alone: setup prints the
  line to add there yourself.
- `--no-hooks` skips this step.
- Undo: `graphicle disconnect` removes the lines. By hand, delete the
  lines between Graphicle's markers.

## Session capture

Session capture saves your Claude Code and Codex conversations in the project,
so that you and your agents can search what was said and decided before.
Setup turns it on; `--no-capture` leaves it off.

- After each answer, a background process stores the session's completed
  turns. The hooks never make the agent wait.
- At the start of a session, the agent receives the titles of your reviewed
  memories and the latest turns, at most 8,000 characters. Reviewed memories
  are decisions, procedures and notes that a person approved in the app.
- Text inside `<private>…</private>` is dropped, and credentials are
  redacted, before anything is stored. No model is called.
- `graphicle sessions forget --client claude-code --session-id ID` forgets one
  session and returns a receipt.

## After an upgrade

Codex, Cursor and Claude Desktop entries, and the capture hooks, start the
program by the path on your `PATH` that leads to it, such as Homebrew's
`/opt/homebrew/bin/graphicle`, which an upgrade keeps, not by the versioned
folder an upgrade removes. If something stopped working anyway (entries an
older Graphicle wrote, or a program you moved), run:

```sh
graphicle doctor --fix
```

`doctor` finds entries, capture hooks and guard hooks that start a program
that no longer exists, entries Graphicle wrote that no longer run
`graphicle mcp` or are gone, and capture hooks that lack an event; `--fix`
rewrites them, and keeps each entry's `--allow-learning` and `--work-state`.
For anything else it reports, run the command it prints.

## Optional: the graph-only server and the graph skill

Setup connects Graphicle's MCP server, `graphicle mcp`. It walks the project's
code graph with the graph engine's tools under Graphify's names (neighbours,
dependency paths, communities and the most connected code), and it searches
your code, documents, reviewed memory and past sessions.
[mcp.md](mcp.md#tools) lists its tools. Two more tools are optional, and
setup does not install them:

- **The graph-only server**, `graphicle graph serve`, serves a `graph.json`
  file on its own, as Graphify's server does, for an agent that should see
  the graph and nothing else. [mcp.md](mcp.md#graph-only-server) shows how
  to add it.
- **The graph skill**, for agents that work through the command line:
  `graphicle graph install` (or `--platform NAME` for another agent).
  `graphicle graph uninstall` removes it.
