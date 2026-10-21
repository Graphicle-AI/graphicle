# Troubleshooting

Start with `graphicle doctor`, in the project folder. It checks the program,
the project and every connected agent, and prints the exact command that fixes
each problem it finds.

```sh
graphicle doctor          # check, and print the fix for each problem
graphicle doctor --fix    # apply the safe fixes
graphicle doctor --json   # the same checks as JSON, for scripts
```

## What doctor checks

| Check | What doctor looks at |
|---|---|
| Program | `graphicle` and its code-graph engine, `graphicle-graph`, are found |
| Data folder | Graphicle can write to it, and no newer Graphicle has used it |
| Project | This folder is set up and synced, nothing changed since, and its code graph is built |
| Agents | Each agent's entry is valid and starts a program that exists; its settings file is valid JSON; the entries setup added are still there; no agent is left connected to a data folder that is gone |
| Hooks | The session capture hooks (each event), Claude Code's graph guard hooks (when `--guards` added them) and the three Git hooks are in place |
| Instructions | The Graphicle section is in `AGENTS.md` or `CLAUDE.md` |
| Model | A model is set for `ask`, and the environment variable that holds its key is set. Doctor never prints the key |
| Port | Port 8854, which the app uses, is free or used by Graphicle |
| Disk | There is enough free disk space |

`--fix` applies the fixes that are safe to run without asking: it syncs the
project again, rebuilds the code graph, repairs agent entries and rewrites
capture and guard hooks. For other problems, run the command that doctor prints.

## Common problems

### `graphicle: command not found`

The install script puts the programs in `~/.local/bin`. If that folder is not
on your `PATH`, add it in your shell profile, then open a new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

With Homebrew, check that `brew --prefix` points at the Homebrew you use.

### The folder is not a project

Commands such as `ask`, `context` and `status` work on the project of the
folder you run them in. Set the folder up first:

```sh
cd your-repo
graphicle setup
```

To store the folder without connecting agents, run `graphicle sync`.

### The repository is too large

A project holds up to 100,000 text files and 1 GiB of text by default. The
message says which limit the folder reached and the setting that raises it.
To fit:

- Raise the limit: `limits.project_files` or `limits.project_bytes` in
  `models.json` in the data folder (`graphicle config path` prints where it
  is), or the `GRAPHICLE_MAX_PROJECT_FILES` or `GRAPHICLE_MAX_PROJECT_BYTES`
  environment variable. [Limits](configuration.md#limits) lists every limit.
- Or leave out folders you do not need. Add them to `.gitignore`, or to
  `.git/info/exclude` to keep the change to your machine. A file Git tracks
  is read whatever these say, unless `graphicle config set
  sync.tracked_ignored false`; an `.ignore` file leaves out tracked files too.
- Or set up a part of the repository: `cd services/api && graphicle setup`.

Graphicle already skips `.git`, `node_modules`, `target`, `dist`, `build`,
virtual environments and secrets such as `.env` files. A text file over 8 MiB
is left out, and a file past an analysis limit is analysed in part and stays
searchable; `graphicle sync` lists both with the reasons. The code graph of
`graphicle graph` has no such limit.

### The port is in use

`graphicle open` serves the app on port 8854. If another program holds that
port, the message gives the program's process ID. Stop that program, or use
another port:

```sh
graphicle open --port 8855
```

If the program on the port is Graphicle, `graphicle open` uses it and opens
your browser there.

### `open` over SSH

The app listens on 127.0.0.1 of the computer it runs on. In an SSH session,
`graphicle open` prints the address instead of opening a browser. Forward the
port from your own computer first, then open the address there:

```sh
ssh -L 8854:127.0.0.1:8854 <host>
```

### `ask` shows sources but no answer

No model is set for answers. Set one with a single command:

```sh
graphicle config set model anthropic   # uses the key in ANTHROPIC_API_KEY
```

`openai` uses `OPENAI_API_KEY`, and `ollama` uses a local Ollama.
[configuration.md](configuration.md#models) covers other providers. If a model
is set but the call fails, `ask` says what failed and how to fix it, for
example that the key's environment variable is missing. `graphicle doctor`
checks the variable.

### `ask` fails although a model is set

`ask` names the cause and the fix. The common ones:

- **The reply reached its output limit.** Raise the profile's limit, for
  example `graphicle config set models.ollama.output_limit 4096`, or ask a
  narrower question.
- **The model quoted text that is not in the sources.** Every answer must
  quote its sources exactly; a quote that differs only in spaces, tabs or
  line breaks is matched to the source's text. Ask again, use
  `graphicle ask "..." --workflow reflect`, which reviews and repairs
  citations, or choose a stronger model.
- **The provider rejected the request.** Its reason is printed in brackets,
  for example a model name it does not know or an input over its context
  window. Lower `models.NAME.input_limit` for a model with a small window.
- **Ollama cannot be reached.** Start it with `ollama serve`. If it listens
  somewhere else, set `OLLAMA_HOST` and run `graphicle config set model
  ollama` again, or pass `--url`.

### An answer says files changed since the last sync

Before it answers, Graphicle checks the project for changes. When a few files
changed, it syncs them first. When many changed, it answers at once and says
how many files changed since the last sync. Catch up with:

```sh
graphicle sync
```

The Git hooks from `graphicle setup` sync the project after each commit, merge
and checkout. If you skipped them, run `graphicle setup` again without
`--no-hooks`.

### The agent does not show Graphicle's tools

1. Restart the agent, or reload its window.
2. Claude Code asks you to approve the servers in a project's `.mcp.json`;
   approve `graphicle`.
3. Codex asks you to trust new hooks before they run.
4. Run `graphicle status`. It lists each connected agent and whether its
   entry works. `graphicle doctor --fix` repairs broken entries.
5. To connect an agent again, run `graphicle connect <agent>`.
   [agents.md](agents.md) lists the agents and their files.

### The agent cannot start Graphicle after an upgrade

Entries and hooks start the program through the link on your `PATH`, which an
upgrade keeps. Entries an older Graphicle wrote may name a versioned path that
an upgrade removed. Run `graphicle doctor --fix` to repair them.

### Doctor says an agent's settings file is not valid JSON

An agent's settings file (for example `.vscode/mcp.json`) has a syntax error,
so none of its servers start. Doctor prints the line and column. Fix the file
by hand; if Graphicle changed it, its copy from before is
`<file>.graphicle-bak`. Then run `graphicle doctor --fix`. VS Code's comments
and trailing commas are fine: Graphicle's entry goes on lines of its own after
the last entry, the other lines keep their comments, and `graphicle disconnect`
keeps comments you change while connected.

### Doctor says agents are connected to a data folder that does not exist

The data folder was moved or deleted while agents still use it, so they fail
to start. Run `graphicle setup` in the repository: it makes a new data folder
and brings the entries up to date.

### The data folder is from a newer version

A newer Graphicle has already used this data folder. This version says so on
the terminal, and `graphicle doctor` reports it. Install the newer version
again; `graphicle update` says how for the way you installed Graphicle. To use
this version with a separate store instead, pass `--data-dir` with another
folder.

### An unknown command

Graphicle suggests the closest command. `graphicle --help` lists the commands
by task, and `graphicle help --all` adds the advanced ones.

## Start over

`graphicle disconnect` removes what Graphicle added for this folder's
project: agent entries, hooks, the instructions section and the Git-hook
lines. `graphicle disconnect --all` does the same for every project.
`graphicle setup` then sets everything up again. To remove
Graphicle completely, see [Uninstall](install.md#uninstall).

## Get help

Report bugs as GitHub issues, with the output of `graphicle doctor --json`. It
never contains keys, but it does contain folder paths, so read it before you
share it. For a security problem, follow [SECURITY.md](../SECURITY.md) instead
of opening a public issue.
