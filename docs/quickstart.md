# Quick start

This page takes you from nothing to your first answer in three steps. You need
macOS or Linux and a code repository on your machine. Nothing here needs an
account, and setup sends none of your code or data over the network.

## 1. Install

```sh
brew install graphicle-ai/tap/graphicle
```

Without Homebrew, use the install script. It puts the programs in
`~/.local/bin`:

```sh
curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh
```

[install.md](install.md) covers release archives and building from source.

## 2. Set up your repository

```sh
cd your-repo
graphicle setup
```

You can also name the folder: `graphicle setup ~/code/my-app`.

Graphicle calls each folder you set up a *project*. Setup does these steps in
order:

1. **Reads the project.** It stores a searchable copy of the folder's files
   and builds the code graph: the files, functions, classes, calls and imports
   of your code. It shows its progress as it goes. It skips what `.gitignore`
   excludes, and secrets such as `.env` files and keys. A folder that is not a
   Git repository works too.
2. **Connects your coding agents.** It finds the agents installed on your
   machine: Claude Code, Codex, Cursor, VS Code and Claude Desktop. It asks
   which ones to connect; the default is all of them. Each agent gets an
   entry that starts Graphicle's MCP server: the code graph's tools (with
   Graphify's names, such as `query_graph`), search over your code, documents
   and memory, and tools that propose memory drafts for you to review. Claude
   Code gets five of the tools (`query_graph`, `get_node`, `get_neighbors`,
   `recall_project`, `read_project_source`); `--tools all` gives it every
   tool.
3. **Turns on session capture** for Claude Code and Codex. After each answer,
   the conversation is saved in the project, so later sessions can search what
   was said and decided. Text inside `<private>…</private>` and credentials
   are removed first.
4. **Adds agent instructions.** A two-line Graphicle section in the
   repository's `AGENTS.md` (and in `CLAUDE.md`, if there is one) tells agents
   when to use Graphicle. If `AGENTS.md` does not exist, setup asks before it
   creates it. When Claude Code is the only agent, setup adds no section: the
   server's own instructions tell Claude Code when to use the tools.
5. **Adds Git hooks** that update the project in the background after each
   commit, merge and checkout. Your own hooks are kept.
6. **Offers to switch from Graphify**, if it finds a Graphify setup. It never
   deletes Graphify's files. See [Moving from Graphify](migrate-from-graphify.md).
7. **Offers a model** to write answers for `graphicle ask`, when it finds one:
   a key in `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`, or a local Ollama. It
   saves the name of the environment variable, never the key. See
   [Add a model](#add-a-model).

The summary at the end lists the project with its file and symbol counts,
each agent with the file that changed, the hooks, the instructions, whether
the project is up to date and the model, and three things to try next.

Outside Graphicle's data folder, setup changes only the files its summary
lists. It backs up each file before the first change, as
`<file>.graphicle-bak`, and it writes no API keys. Run it
again at any time: it reports what is already set and changes nothing else.
[agents.md](agents.md) lists every file it writes for each agent, and how to
undo it.

### Setup options

| Option | Effect |
|---|---|
| `--yes` | Accept the defaults without asking |
| `--clients claude-code,cursor` | Connect only these agents. `all` connects every agent found, `none` connects no agent |
| `--no-capture` | Skip session capture |
| `--no-instructions` | Leave `AGENTS.md` and `CLAUDE.md` alone |
| `--no-hooks` | Add no Git hooks |
| `--tools all` | Give Claude Code every tool of the server, not five (`essentials`, the default for Claude Code) |
| `--guards` | Add hooks that point Claude Code's searches and source reads to the graph tools (off by default) |
| `--switch-from-graphify` | Switch from Graphify without asking |
| `--json` | Print the result as JSON, for scripts |

When setup does not run in a terminal (in a script, for example), it asks
nothing: pass `--yes` to accept the defaults, or `--clients` to choose the
agents.

## 3. Use it

### In your agent

Restart your agent, or reload its window, so that it starts Graphicle. Then
ask in plain words, and mention Graphicle:

- "Use graphicle to find where failed requests are retried."
- "Use graphicle: what did we decide about the database migration?"
- "Use graphicle to show what calls `parse_config` before you change it."

The agent calls Graphicle's tools over MCP, the protocol coding agents use to
call tools. The tools walk the code graph and return cited excerpts from your
code, your documents and earlier sessions. Your agent's own model writes the
answer; Graphicle's tools call no model. When an agent proposes a memory, it
waits as a draft until you approve it in the app (`graphicle open`, then
**Memory**).

### In the terminal

```sh
graphicle ask "Where are failed requests retried?"      # an answer with numbered citations
graphicle context "Where are failed requests retried?"  # the cited excerpts only, without a model
graphicle open                                          # the app in your browser
graphicle status                                        # the project, its agents and hooks
```

`graphicle` alone, inside a project, shows its status and the next commands.
`graphicle --help` lists the commands by task.

On a terminal these commands print readable text. In a pipe, they print
JSON for scripts.

### Add a model

`graphicle ask` needs a model to write an answer. Without one, it shows the
cited excerpts and the command that adds a model. One command sets it:

```sh
graphicle config set model anthropic   # uses the key in ANTHROPIC_API_KEY
graphicle config set model openai      # uses the key in OPENAI_API_KEY
graphicle config set model ollama      # a local Ollama, no key
```

Graphicle saves the name of the environment variable, never the key. Keep
the key in your shell profile. [configuration.md](configuration.md#models)
covers other providers and local servers.

## Staying up to date

You do not need to sync by hand. Before Graphicle answers (`ask`, `context`,
`status` and every agent tool call), it checks whether files changed since the
last sync:

- If a few files changed, it syncs them first, which takes a moment.
- If many files changed, it answers at once and says how many files changed
  since the last sync. Run `graphicle sync` to catch up.

The Git hooks that setup adds sync the project after each commit, merge and
checkout. To sync by hand, run `graphicle sync` anywhere in the project: in
a subfolder it syncs the whole project. To turn the
check off, run `graphicle config set freshness off`.

## When something goes wrong

```sh
graphicle doctor
```

`doctor` checks the program, the project, your agents' entries, the hooks and
the model, and prints the command that fixes each problem.
`graphicle doctor --fix` applies the safe fixes for you.
[troubleshooting.md](troubleshooting.md) covers the common problems.

## Next steps

- [Agents](agents.md): what each agent gets, and how to disconnect it
- [Moving from Graphify](migrate-from-graphify.md)
- [A shared server for your team](team.md)
- [MCP](mcp.md): every tool, and other MCP clients
- [Configuration](configuration.md): models, the data folder and retrieval
