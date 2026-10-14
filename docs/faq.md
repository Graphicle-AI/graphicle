# FAQ

## What is the difference between the code graph and the memory?

The code graph (`graphicle graph`) records how the code fits together:
definitions, calls, imports, inheritance and communities of related code. It
lives in `.graphicle/graph.json` in each project.

The memory (`graphicle sync`, `context`, `ask`) keeps the project's sources
word for word: code, documents, captured agent sessions and connector content.
It also keeps memories that a person reviewed, such as decisions, preferences
and procedures. It lives in the data folder.

Use the graph for questions about the code's structure, and the memory for
what the project says and decided. Agents can use both; see [mcp.md](mcp.md).

## Do I need an API key or a model?

No, for most of Graphicle. These work without one: the code graph (with
`--code-only`), sync, `context`, both MCP servers, session capture and the
app.

A model is needed for:

- `graphicle ask` and the app's **Ask** box;
- the jobs that propose memory from sources (`extract`, `refresh` and
  `observer_notes`);
- a full `graphicle graph extract` of documents, papers and images;
- embeddings.

Local models through Ollama or any OpenAI-compatible server work. See
[configuration.md](configuration.md#models).

## Does Graphicle send my code anywhere?

Not unless you set it up to. There is no telemetry and no account. The app
listens on 127.0.0.1. Graphicle sends text to a model only when you configure
one for answers or extraction. A full `graphicle graph extract` without
`--code-only` sends documents, papers and images to the LLM backend that it
finds through an API key in your environment. Run `--dry-run` first to see what
it would send. The README's [privacy section](../README.md#privacy-and-local-first)
lists every case.

## Is Graphicle a fork of Graphify?

The code-graph engine is a Rust port of Graphify, which is
licensed under Apache-2.0. Its source files say so in their headers, and
`NOTICE` carries the upstream attribution. It is kept in step with Graphify's
releases: comparison checks run Graphify's own tests against the engine and
compare its results with Graphify's, with the same reviewed upstream fixes on
both sides ([the parity checks](benchmarks.md#parity-with-graphify): the
release the engine follows, what they cover, their results, their limits and
the known differences).
A pre-release build of the engine, which then followed Graphify 0.9.77, ran
faster than Graphify 0.9.77 in our measurements
([benchmarks.md](benchmarks.md#speed-against-graphify-0977)). It also has
opt-in additions: precise call resolution, documentation and route links,
cited retrieval and a live graph server.

The memory, the app, session capture and the memory half of Graphicle's MCP
server are not part of Graphify; the server's graph tools keep Graphify's
names. Where they adapt other open-source work, the notices credit it.

## I use Graphify. How do I switch?

Run `graphicle setup` in the repository. It finds your Graphify setup, offers
to replace Graphify's MCP entry and instructions with Graphicle's, and backs up
each file first. It never deletes `graphify-out/` or Graphify's skill.
[migrate-from-graphify.md](migrate-from-graphify.md) has the details.

## Which agents does it work with?

Any MCP client.

- `graphicle setup` connects the agents it finds, and `graphicle connect`
  writes the settings for one: Claude Code (`claude-code`), Cursor
  (`cursor`), VS Code (`vscode`), Codex (`codex`) and Claude Desktop
  (`claude`). [agents.md](agents.md) lists what each one gets and how to undo
  it. Other clients take a short JSON entry ([mcp.md](mcp.md#other-clients)).
- Session capture supports Claude Code and Codex. Setup turns it on.
- `graphicle graph install --platform NAME` installs the graph's skill and
  rules for about 20 agents.

## How do agents write memory?

Only as drafts for a person to review.

- **Proposals.** With `graphicle mcp --allow-learning`, which `graphicle
  setup` turns on, an agent can propose a memory, such as a decision,
  preference, procedure, task state or note. A quote inside one function,
  class or method anchors the memory to it: the memory goes stale when that
  code changes, and edits elsewhere in the file usually leave it current
  ([the exact rule](../crates/graphicle/README.md#memory-anchored-to-code-symbols)).
  Every quote in the proposal must appear word for word in a current source. The
  proposal stays a draft until a person approves it in the app, under
  **Memory**, and recall never serves a draft.
- **Captured sessions.** Turns captured by session capture are sources, not
  memories. Agents can search and cite them.
- **Work state.** `--work-state` gives the agent its own to-do lists. Recall
  never uses them as memory.

## My repository is large. Will sync handle it?

Yes. `graphicle sync` takes up to 100,000 text files and 1 GiB of text per
project, and you can raise both: set `limits.project_files` and
`limits.project_bytes` in `models.json`, or `GRAPHICLE_MAX_PROJECT_FILES` and
`GRAPHICLE_MAX_PROJECT_BYTES`. A larger folder stops with a message that names
the count, the limit and the setting. You can also extend `.gitignore` or sync
a subfolder.

One large or unusual file never stops a sync. A file over 4 MiB, or with more
than 50,000 calls or 2,000,000 syntax nodes, is analysed as far as the limit
allows and stays searchable. A text file over 8 MiB, or one that cannot be
read, is left out. The result of `sync` lists every such file with the reason.
[Configuration](configuration.md#limits) lists every limit and its setting.

One question searches at most 50,000 files and about 100 MB of text. In a
larger project each question chooses the files that match it best, and the
sync says so; pass `include` to choose the files yourself. Before it chooses,
a question still reads the text of every file into memory: a 60,000-file
project used about 1.1 GiB per question when this was measured. The code
graph has no such limits.

## Where is my data, and how do I delete it?

Everything is in the data folder (`~/Library/Application Support/Graphicle`
on macOS, `~/.local/share/graphicle` on Linux) and in `.graphicle/` in each
project.

- `graphicle sessions forget --client claude-code --session-id ID` forgets one
  captured session, returns a hash-chained receipt and stops the capture of that
  session's later turns.
- `graphicle call project.delete --arguments delete.json` deletes the current
  project and returns a hash-chained receipt. The file holds
  `{"expected_snapshot": "…"}`, the `repository.snapshot` that
  `graphicle status` prints. Backups you made are separate.
- The app retires memories and deletes connectors.
- To remove everything, delete the data folder ([install.md](install.md#uninstall)).

## Does it run on Windows?

Not yet. Release builds are for macOS and Linux.

## My agent cannot start Graphicle after an upgrade

`graphicle connect` stores the path on your `PATH` that leads to the program
(Homebrew's `bin/graphicle`, for example), which an upgrade keeps; in a
folder's file (Claude Code, VS Code) it stores just `graphicle`. Entries an
older Graphicle wrote may name a versioned folder that an upgrade removed: run
`graphicle doctor --fix` to repair them
([install.md](install.md#after-an-upgrade)).

## Is there an SDK?

Yes. `sdk/python` (`graphicle-sdk`, imported as `graphicle_sdk`) and
`sdk/typescript` talk to a running `graphicle serve`. Neither is published to
a package registry yet, so install them from the repository, for example
with `python -m pip install ./sdk/python`.

## Is there a version for teams?

Graphicle is free and open source under Apache-2.0, for individuals and teams alike. A commercial edition for teams is in private preview. To talk to us, start a discussion: https://github.com/graphicle-ai/graphicle/discussions, or chat with us on [Discord](https://discord.com/invite/rWuU7cWU4E).

## How do I report a bug or a security problem?

Report bugs as GitHub issues. For security problems, follow
[SECURITY.md](../SECURITY.md) rather than opening a public issue.
