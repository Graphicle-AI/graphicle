# Moving from Graphify

Graphicle's code-graph engine is a Rust port of Graphify. If you use Graphify,
most of what you know carries over. This page lists what stays the same, what
differs, and how `graphicle setup` switches a repository.

## What stays the same

- **The graph.** The engine is a Rust port of Graphify, so it reads
  the same languages and builds the same kind of graph: files, functions,
  classes, calls, imports and communities of related code.
  [benchmarks.md](benchmarks.md) describes how its results were compared with
  Graphify's.
- **The commands and flags**, under the `graphicle graph` command. For
  example, `graphify query "..."` becomes `graphicle graph query "..."`.
- **The MCP tool names.** The MCP server that setup connects,
  `graphicle mcp`, has Graphify's graph tools under their names:
  `query_graph`, `get_node`, `get_neighbors`, `get_community`, `god_nodes`,
  `graph_stats` and `shortest_path`, in the same text format, and the same
  resources (`graphicle-graph://report` and the others). Rules and prompts
  that name these tools keep working. They read the project's code graph that
  `graphicle sync` builds, which can differ in detail from a `graph.json` of
  the same code ([mcp.md](mcp.md#tools) says how).
- **Ignore files** use the `.gitignore` syntax, as before.
- **No model for code.** A code-only graph needs no model and no API key.

## What differs

| | Graphify | Graphicle |
|---|---|---|
| Install | A Python package | One native program, no Python needed |
| Graph commands | `graphify <command>` | `graphicle graph <command>`, or `graphicle-graph <command>` |
| Output folder | `graphify-out/` | `.graphicle/`; `GRAPHICLE_OUT` sets another |
| Ignore file | `.graphifyignore` | `.graphicleignore` |
| Agent skill | `~/.claude/skills/graphify` | Graphicle's own, from `graphicle graph install` |
| Agent setup | One command per agent | `graphicle setup` connects every agent it finds |

Graphicle also does more than the graph:

- **Memory.** It keeps a searchable copy of your code and documents, your past
  agent sessions, and decisions that a person reviewed. `graphicle ask` and
  `graphicle context` answer from all of them, with citations, and so does
  `recall_project`, in the same MCP server as the graph tools. A reviewed
  decision quoted from a function is anchored to that function.
- **Always current.** Git hooks and a check before each answer keep the
  project up to date.
- **An app.** `graphicle open` shows the graph, the sources and the memory in
  your browser.

## Switch a repository

Run setup in the repository:

```sh
cd your-repo
graphicle setup
```

Setup looks for five signs of Graphify:

- a `graphify-out/` folder in the repository;
- an MCP entry named `graphify` (or `graphify-…`) in an agent's settings;
- a Graphify section in `CLAUDE.md` or `AGENTS.md`: the `## graphify`
  heading and its text, up to the next heading of level one or two;
- Graphify's skill for Claude Code, in `~/.claude/skills/graphify`;
- Graphify's hook in the repository's Claude Code settings
  (`.claude/settings.json`), which `graphify claude install` adds.

It tells you what it found, with the lines of each section it would replace,
and asks whether to switch. If you say yes, setup:

- connects Graphicle in each agent that has a Graphify entry, then removes
  Graphify's entry there (only once Graphicle's is in place);
- puts Graphicle's section in the place of the Graphify section; your own
  text after it stays;
- backs up each file first, as `<file>.graphicle-bak`.

Setup does not edit two places, and says so:

- a Graphify entry in Claude Code's own settings (`~/.claude.json`, which
  Claude Code rewrites while it runs): setup prints the `claude mcp remove`
  command that removes it;
- Graphify's hook in `.claude/settings.json`, a file your team may share:
  remove it yourself (see below).

Setup never deletes `graphify-out/` or Graphify's skill. Graphicle does not
read `graphify-out/`, and its sync skips that folder, so you can keep it or
delete it yourself.

If you say no, setup connects Graphicle beside Graphify, and your agents can
use both. When setup does not run in a terminal, it switches only when you
pass `--switch-from-graphify`.

## After the switch

- Restart your agents, so that they load Graphicle's tools.
- Graphify's skill stays installed, and Claude Code can still offer it. When
  you no longer need it, delete the folder `~/.claude/skills/graphify`.
- If you installed Graphify's Git hooks, remove them with
  `graphify hook uninstall`. Graphicle's own hooks keep the project current.
- Graphify's Claude Code hook stays in `.claude/settings.json` and still asks
  Claude to read `graphify-out/GRAPH_REPORT.md` before every search. When you
  no longer use Graphify, delete that `PreToolUse` entry (the one whose
  command mentions `graphify-out`) from the file's `hooks`. Setup gives
  Claude Code Graphicle's own guard hooks, in `.claude/settings.local.json`,
  which point its searches to the graph tools of `graphicle mcp`
  ([agents.md](agents.md#connect-and-disconnect)).
- Setup's code graph lives in Graphicle's data folder: it serves `graphicle
  ask` and your agents, and you need nothing else for them. Only the
  `graphicle graph` commands read a graph file. To keep using the ones you
  know, build that file once:

  ```sh
  graphicle graph extract . --code-only
  ```

  Then `graphicle graph query`, `explain`, `path` and `affected` work as they
  did with Graphify, on `.graphicle/graph.json`. Add `.graphicle/` to your
  `.gitignore`, as you had `graphify-out/`.

## Commands side by side

| Graphify | Graphicle |
|---|---|
| `graphify update .` | `graphicle graph update .` |
| `graphify query "question"` | `graphicle graph query "question"` |
| `graphify explain "symbol"` | `graphicle graph explain "symbol"` |
| `graphify path "A" "B"` | `graphicle graph path "A" "B"` |
| `graphify serve graphify-out/graph.json` | The graph tools of `graphicle mcp`, which setup connects; for a graph file alone, `graphicle graph serve .graphicle/graph.json` |
| `graphify install` | `graphicle graph install` |
| `graphify hook install` | `graphicle graph hook install` |

## Going back

Run `graphicle disconnect` in the repository. It removes what Graphicle added
and puts back the Graphify entries and section that the switch replaced.
Each `<file>.graphicle-bak` also holds the file as it was before Graphicle
first changed it.
